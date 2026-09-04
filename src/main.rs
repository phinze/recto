mod app;
mod backend;
mod cli;
mod diff;
mod funcname;
mod github;
mod graph;
mod highlight;
mod input;
mod link;
mod markdown;
mod state;
#[cfg(test)]
mod testing;
mod theme;
mod ui;
mod watch;
mod wrap;

use std::io::{self, stdout};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use std::time::Instant;

use anyhow::{Result, anyhow};
use clap::Parser;
use crossterm::{
    event::{self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::app::{App, POLL_INTERVAL, RELOAD_DEBOUNCE};
use crate::backend::detect_backend;
use crate::cli::{ClientCommand, run_client};
use crate::highlight::Highlighter;
use crate::input::handle_event;
use crate::ui::chrome::draw;

/// recto — a jj-first terminal diff viewer.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Drive a running recto for this workspace instead of opening the TUI.
    #[command(subcommand)]
    command: Option<ClientCommand>,

    /// Initial diff base (jj revset or git ref). Examples: `@-`, `trunk()`, `HEAD`.
    #[arg(long, value_name = "REVSET")]
    base: Option<String>,

    /// Run as if started from this directory. Matches jj's `-R`.
    #[arg(short = 'R', long, value_name = "PATH")]
    repository: Option<std::path::PathBuf>,
}

#[derive(serde::Deserialize)]
struct RigInfo {
    schema_version: u32,
    #[serde(default)]
    root: Option<std::path::PathBuf>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    review_pr: Option<String>,
}

/// Ask Rig for the current repository's public context. A missing binary or a
/// non-rig cwd is the ordinary standalone case and returns no PR. Successful
/// output is a versioned API, so malformed JSON is worth surfacing instead of
/// silently treating a broken integration as "not a review".
fn info_from_rig(repo_root: &Path) -> Result<Option<RigInfo>> {
    let output = match Command::new("rig")
        .args(["info", "--format=json"])
        .current_dir(repo_root)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!("could not ask rig for review context: {error}")),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let info: RigInfo = serde_json::from_slice(&output.stdout)
        .map_err(|error| anyhow!("rig info returned invalid JSON: {error}"))?;
    if info.schema_version != 1 {
        return Err(anyhow!(
            "rig info schema {} is not supported",
            info.schema_version
        ));
    }
    Ok(Some(info))
}

fn main() -> Result<()> {
    let _ = color_eyre::install();
    let cli = Cli::parse();

    if let Some(path) = &cli.repository {
        std::env::set_current_dir(path).unwrap_or_else(|e| {
            eprintln!("recto: -R {}: {e}", path.display());
            std::process::exit(2);
        });
    }

    if let Some(command) = cli.command {
        std::process::exit(run_client(command));
    }

    let backend = detect_backend().unwrap_or_else(|e| {
        eprintln!("recto: {e}");
        std::process::exit(2);
    });
    std::env::set_current_dir(backend.root()).unwrap_or_else(|e| {
        eprintln!("recto: repository root {}: {e}", backend.root().display());
        std::process::exit(2);
    });

    let mut startup_notices = Vec::new();
    let rig_info = match info_from_rig(backend.root()) {
        Ok(info) => info,
        Err(error) => {
            startup_notices.push(error.to_string());
            None
        }
    };
    let legacy_rig = rig_info
        .as_ref()
        .and_then(|info| info.root.as_deref().zip(info.repo.as_deref()));
    let persistence = match state::Store::load(backend.root(), legacy_rig) {
        Ok(store) => Some(store),
        Err(error) => {
            startup_notices.push(format!("could not restore review state: {error}"));
            None
        }
    };
    // A review rig's freshly fetched PR wins: it is newer than whatever the
    // last session saved. Everywhere else the saved snapshot is restored from
    // disk, so an ordinary startup still makes no network call.
    let pull_request = match rig_info.as_ref().and_then(|info| info.review_pr.as_ref()) {
        Some(locator) => match github::fetch_pull_request(locator) {
            Ok(pull_request) => Some(pull_request),
            Err(error) => {
                startup_notices.push(format!("could not restore rig review {locator}: {error}"));
                None
            }
        },
        None => persistence
            .as_ref()
            .and_then(|store| store.pull_request().cloned()),
    };
    // An explicit base remains an escape hatch. Otherwise a review rig starts
    // from GitHub's recorded base commit instead of a moving branch name.
    let initial_base = cli
        .base
        .or_else(|| pull_request.as_ref().map(|pr| pr.base_oid.clone()));
    let hl = Highlighter::new();
    let mut app = App::load(backend, hl, initial_base, persistence).unwrap_or_else(|e| {
        eprintln!("recto: {e}");
        std::process::exit(2);
    });
    if let Some(pull_request) = pull_request {
        let response = app.attach_pull_request(pull_request, false);
        debug_assert!(response.ok);
    }
    if !startup_notices.is_empty() {
        app.load_error = Some(startup_notices.join("; "));
    }

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal()?;
    result
}

/// Split `path`, `path:LINE`, or `path:START-END` into its parts. Only treats
/// the tail after the last `:` as a range when it actually parses as one, so
/// paths that happen to contain a colon survive.
fn parse_pathspec(spec: &str) -> (&str, Option<u32>, Option<u32>) {
    if let Some((path, tail)) = spec.rsplit_once(':')
        && let Some((start, end)) = parse_range(tail)
    {
        return (path, Some(start), end);
    }
    (spec, None, None)
}

fn parse_range(tail: &str) -> Option<(u32, Option<u32>)> {
    match tail.split_once('-') {
        Some((a, b)) => Some((a.parse().ok()?, Some(b.parse().ok()?))),
        None => Some((tail.parse().ok()?, None)),
    }
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enter_terminal()?;
    Ok(Terminal::new(CrosstermBackend::new(stdout()))?)
}

fn enter_terminal() -> Result<()> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        SetTitle(terminal_title()),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange
    )?;
    Ok(())
}

fn terminal_title() -> String {
    let root = std::env::current_dir()
        .ok()
        .and_then(|cwd| link::workspace_root(&cwd).or(Some(cwd)));
    root.as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(|name| format!("recto: {name}"))
        .unwrap_or_else(|| "recto".into())
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(
        stdout(),
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    Ok(())
}

fn run_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &str,
    line: u32,
    editor_link: &link::EditorLink,
    status: link::Status,
) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("vi");
    let extra_args: Vec<&str> = parts.collect();

    // If the editor is neovim, hand it an RPC socket via `--listen` so the
    // agent link can drive its cursor and highlight while we're parked here.
    // Anything else gets the plain handoff; focus is deferred to our return.
    let nvim_addr = if editor_is_nvim(prog) {
        let addr = link::nvim_addr(std::process::id());
        let _ = std::fs::remove_file(&addr);
        Some(addr)
    } else {
        None
    };

    restore_terminal()?;
    editor_link.enter(
        nvim_addr.as_ref().map(|addr| link::NvimHandle {
            prog: prog.to_string(),
            addr: addr.clone(),
        }),
        status,
    );

    let mut cmd = Command::new(prog);
    cmd.args(&extra_args);
    if let Some(addr) = &nvim_addr {
        cmd.arg("--listen").arg(addr);
    }
    let _ = cmd.arg(format!("+{line}")).arg(path).status();

    editor_link.leave();
    if let Some(addr) = &nvim_addr {
        let _ = std::fs::remove_file(addr);
    }

    enter_terminal()?;
    terminal.clear()?;
    Ok(())
}

/// Whether `prog` is neovim, and thus speaks `--listen`/`--remote-expr`. Plain
/// vim's `+clientserver` is usually absent in terminal builds, so we gate the
/// live-drive path on neovim specifically and let everything else fall back.
fn editor_is_nvim(prog: &str) -> bool {
    Command::new(prog)
        .arg("--version")
        .output()
        .map(|o| o.status.success() && o.stdout.starts_with(b"NVIM"))
        .unwrap_or(false)
}

enum Action {
    Continue,
    Quit,
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    let (tx, rx) = mpsc::channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && watch::is_interesting(&event)
        {
            let _ = tx.send(event);
        }
    })?;
    let mut watch_tree = watch::WatchTree::new(".");
    watch_tree.refresh(&mut watcher);

    // Agent link: companion sessions reach us over a per-workspace socket.
    // A bind failure shouldn't sink the TUI — the link is best-effort.
    // `editor_link` lets the listener drive a live neovim (and stay responsive)
    // while we're parked in the `e` editor handoff.
    let editor_link = Arc::new(link::EditorLink::default());
    let link_rx = link::socket_for_cwd()
        .and_then(|p| link::spawn_listener(&p, editor_link.clone()))
        .ok();

    let mut pending_reload: Option<Instant> = None;
    let mut needs_redraw = true;

    loop {
        needs_redraw |= app.poll_load();
        needs_redraw |= app.poll_persistence();

        if let Some(link_rx) = &link_rx {
            while let Ok(incoming) = link_rx.try_recv() {
                let mut resp = app.handle_request(incoming.request);
                if resp.ok
                    && app.persistence_due.is_some()
                    && let Err(error) = app.persist_now()
                {
                    resp = link::Response::err(format!(
                        "state changed in memory but could not be saved: {error}"
                    ));
                }
                let _ = incoming.respond.send(resp);
                needs_redraw = true;
            }
        }

        let mut refresh_watches = false;
        while let Ok(event) = rx.try_recv() {
            pending_reload = Some(Instant::now());
            refresh_watches |= watch::may_add_directories(&event);
        }
        if refresh_watches {
            watch_tree.refresh(&mut watcher);
        }
        if let Some(t) = pending_reload
            && t.elapsed() >= RELOAD_DEBOUNCE
        {
            needs_redraw |= app.request_reload();
            pending_reload = None;
        }

        if needs_redraw || app.is_animating() {
            let selected_before = app.file_state.selected();
            terminal.draw(|f| draw(f, app))?;
            // `draw_diff` keeps the file selection synchronized with the
            // current scroll position, after the file pane has already been
            // rendered. Give that selection change one follow-up frame.
            needs_redraw = selected_before != app.file_state.selected();
        }

        if event::poll(POLL_INTERVAL)? {
            if matches!(
                handle_event(app, terminal, event::read()?, &editor_link)?,
                Action::Quit
            ) {
                app.persist_now()?;
                break;
            }
            needs_redraw = true;
            // Coalesce bursts (key autorepeat, mouse-scroll) into one redraw
            // by draining everything already queued before drawing again.
            while event::poll(Duration::ZERO)? {
                if matches!(
                    handle_event(app, terminal, event::read()?, &editor_link)?,
                    Action::Quit
                ) {
                    app.persist_now()?;
                    return Ok(());
                }
                needs_redraw = true;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rig_info_review_locator_is_a_narrow_json_contract() {
        let info: RigInfo = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "id": "pr-42",
                "root": "/tmp/pr-42",
                "kind": "review",
                "repo": "repo",
                "repository": "owner/repo",
                "review_pr": "https://github.com/owner/repo/pull/42"
            }"#,
        )
        .unwrap();
        assert_eq!(
            info.review_pr.as_deref(),
            Some("https://github.com/owner/repo/pull/42")
        );
        assert_eq!(info.schema_version, 1);
        assert_eq!(info.root.as_deref(), Some(Path::new("/tmp/pr-42")));
    }

    #[test]
    fn pathspec_range() {
        assert_eq!(
            parse_pathspec("src/main.rs:12-20"),
            ("src/main.rs", Some(12), Some(20))
        );
    }

    #[test]
    fn pathspec_single_line() {
        assert_eq!(
            parse_pathspec("src/main.rs:12"),
            ("src/main.rs", Some(12), None)
        );
    }

    #[test]
    fn pathspec_whole_file() {
        assert_eq!(parse_pathspec("src/main.rs"), ("src/main.rs", None, None));
    }

    #[test]
    fn pathspec_colon_in_path_without_range() {
        // A trailing colon segment that isn't a number is part of the path.
        assert_eq!(parse_pathspec("weird:name"), ("weird:name", None, None));
    }
}
