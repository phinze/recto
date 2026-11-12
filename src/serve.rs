//! `recto serve`: a read-only web rendering of one or more workspaces, spoken
//! as HTTP over a unix socket.
//!
//! One serve process answers for every workspace on the host. A request names
//! the workspaces it wants with repeated `root=` query parameters, so the unit
//! a page shows is whatever set of repositories the caller binds together (a
//! rig's repos, in practice) rather than one repository per process. Recto
//! never learns who the caller is or why those roots belong together.
//!
//! Serve reads; it never writes. Authored state (the tour, notes, an attached
//! PR) comes from the same durable store a TUI saves to, so a page renders the
//! same whether or not a viewer is running, and a running viewer stays the
//! only writer. The diff is computed here from the same backend the TUI uses.
//!
//! The socket is the only door. It lives in recto's 0700 runtime directory and
//! is itself 0600, so file permissions are the authentication; anything that
//! wants to put this on a network (rig serve, over the tailnet) fronts it.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use notify::{RecursiveMode, Watcher};
use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier};

use crate::backend::{
    Backend, Base, Commit, Scope, detect_backend_from, repository_root, short_oid,
};
use crate::highlight::{Highlighter, expand_tabs, ext_for_path};
use crate::watch::{WatchTree, is_interesting, may_add_directories};
use crate::{link, markdown, parse_pathspec, state};

/// Lines past this many in one response render without syntax highlighting,
/// so a whole-repo diff still answers promptly.
const HIGHLIGHT_BUDGET: usize = 20_000;
/// A repo whose whole diff runs to at most this many lines opens with every
/// file expanded; past it, files start collapsed to a list with diffstats.
const OPEN_DIFF_LINES: usize = 400;
/// Quiet period after a filesystem event before telling the page to refresh,
/// so an agent's burst of writes becomes one refresh.
const SETTLE: Duration = Duration::from_millis(400);
const SETTLE_MAX: Duration = Duration::from_secs(2);
/// SSE comment interval. Doubles as dead-client detection, since a write to a
/// closed connection is how a stream learns to stop.
const KEEPALIVE: Duration = Duration::from_secs(15);

pub(crate) fn default_socket() -> PathBuf {
    link::runtime_dir().join("serve.sock")
}

/// Bind and serve forever. Returns a process exit code only on failure.
pub(crate) fn run(socket: Option<PathBuf>) -> i32 {
    let path = socket.unwrap_or_else(default_socket);
    match bind(&path) {
        Ok(listener) => {
            eprintln!("recto serve: listening on {}", path.display());
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        thread::spawn(move || handle(stream));
                    }
                    // A persistent failure (out of descriptors, say) would
                    // otherwise spin; back off and let connections drain.
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
            0
        }
        Err(e) => {
            eprintln!("recto serve: {e:#}");
            2
        }
    }
}

fn bind(path: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
        link::harden_dir(parent);
    }
    if path.exists() {
        std::fs::remove_file(path).ok();
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    Ok(listener)
}

struct Request {
    path: String,
    roots: Vec<String>,
    /// The directory the roots were bound together under, when the caller has
    /// one. Its own tour reads across every root.
    group: Option<String>,
    label: Option<String>,
    /// Where each root's work started, as `(root, commit)`: the commit a rig
    /// made its workspace from. A page can anchor there instead of at the
    /// branch point once that work has landed on trunk.
    starts: Vec<(String, String)>,
    /// A narrowed view per root index: one commit's own diff in place of the
    /// whole range.
    views: Vec<(usize, String)>,
}

fn read_request(stream: &UnixStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" {
        return None;
    }
    // Drain headers; nothing in them changes the answer.
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header.trim().is_empty() {
            break;
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut roots = Vec::new();
    let mut label = None;
    let mut group = None;
    let mut starts = Vec::new();
    let mut views = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "root" => roots.push(percent_decode(v)),
            "label" => label = Some(percent_decode(v)),
            "group" => group = Some(percent_decode(v)).filter(|g| !g.is_empty()),
            "start" => {
                if let Some((root, commit)) = percent_decode(v).rsplit_once(':') {
                    starts.push((root.to_string(), commit.to_string()));
                }
            }
            "view" => {
                if let Some((i, spec)) = percent_decode(v).split_once(':')
                    && let Ok(i) = i.parse()
                {
                    views.push((i, spec.to_string()));
                }
            }
            _ => {}
        }
    }
    Some(Request {
        path: path.to_string(),
        roots,
        group,
        label,
        starts,
        views,
    })
}

fn handle(mut stream: UnixStream) {
    let Some(req) = read_request(&stream) else {
        respond(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"bad request\n",
        );
        return;
    };
    match req.path.as_str() {
        "/" | "/index.html" => {
            let body = page(&req);
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                body.as_bytes(),
            );
        }
        "/fragment" => {
            let body = fragment(&req);
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                body.as_bytes(),
            );
        }
        "/events" => events(stream, &req),
        "/healthz" => respond(&mut stream, "200 OK", "text/plain", b"ok\n"),
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found\n"),
    }
}

fn respond(stream: &mut UnixStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

// ---------------------------------------------------------------------------
// Live updates

/// Stream `changed` events whenever a requested workspace's authored state or
/// working tree moves. The page answers by fetching a fresh fragment, so the
/// event carries no payload and a missed one costs only staleness until the
/// next.
fn events(mut stream: UnixStream, req: &Request) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Accel-Buffering: no\r\n\r\nretry: 2000\n\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let Ok(mut watcher) = notify::recommended_watcher(tx) else {
        let _ = stream.write_all(b"event: error\ndata: could not watch the filesystem\n\n");
        return;
    };

    let mut state_files = HashSet::new();
    let mut watch_state = |dir: &Path| {
        if let Ok(path) = state::state_path(dir) {
            if let Some(parent) = path.parent() {
                // The store renames into place, so watch the directory; the
                // file itself may not exist until the first save.
                std::fs::create_dir_all(parent).ok();
                let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
            }
            state_files.insert(path);
        }
    };
    let roots: Vec<PathBuf> = req
        .roots
        .iter()
        .filter_map(|r| canonical_repo(Path::new(r)))
        .collect();
    for root in &roots {
        watch_state(root);
    }
    if let Some(group) = &req.group {
        watch_state(Path::new(group));
    }
    let mut trees = Vec::new();
    for root in roots {
        let mut tree = WatchTree::new(root);
        tree.refresh(&mut watcher);
        trees.push(tree);
    }

    let relevant = |event: &notify::Event| {
        is_interesting(event)
            && event.paths.iter().any(|p| {
                if state_files.contains(p) {
                    return true;
                }
                let s = p.to_string_lossy();
                // The diff itself snapshots jj's working copy, which writes
                // under .jj; reacting to that would loop forever.
                let vcs = s.contains("/.jj/") || s.contains("/.git/") || s.ends_with("/.jj");
                // Another workspace's state file shares the directory.
                let other_state = p
                    .parent()
                    .is_some_and(|d| state_files.iter().any(|f| f.parent() == Some(d)));
                !vcs && !other_state
            })
    };

    loop {
        match rx.recv_timeout(KEEPALIVE) {
            Ok(Ok(event)) if relevant(&event) => {
                let mut grow = may_add_directories(&event);
                let started = Instant::now();
                while started.elapsed() < SETTLE_MAX {
                    match rx.recv_timeout(SETTLE) {
                        Ok(Ok(e)) => grow |= may_add_directories(&e),
                        Ok(Err(_)) => {}
                        Err(_) => break,
                    }
                }
                if grow {
                    for tree in &mut trees {
                        tree.refresh(&mut watcher);
                    }
                }
                if stream.write_all(b"event: changed\ndata: 1\n\n").is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream.write_all(b": keepalive\n\n").is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering

fn highlighter() -> &'static Highlighter {
    static HL: OnceLock<Highlighter> = OnceLock::new();
    HL.get_or_init(Highlighter::new)
}

/// The canonical root of the repository at exactly `path`. A subdirectory of
/// a repository is refused rather than silently widened to the whole repo.
fn canonical_repo(path: &Path) -> Option<PathBuf> {
    let canon = path.canonicalize().ok()?;
    (repository_root(&canon)? == canon).then_some(canon)
}

fn page(req: &Request) -> String {
    let title = req.label.as_deref().unwrap_or("recto");
    let mut out = String::with_capacity(64 * 1024);
    out.push_str("<!doctype html><html lang=en><head><meta charset=utf-8>");
    out.push_str("<meta name=viewport content=\"width=device-width,initial-scale=1\">");
    let _ = write!(
        out,
        "<title>{}</title><style>{CSS}</style></head><body>",
        esc(title)
    );
    out.push_str("<div id=live title=\"live updates\"></div><main id=content>");
    out.push_str(&fragment(req));
    let _ = write!(out, "</main><script>{JS}</script></body></html>");
    out
}

/// Everything inside `<main>`, rendered per workspace in parallel since each
/// one shells out to its VCS.
fn fragment(req: &Request) -> String {
    let roots = &req.roots;
    if roots.is_empty() {
        return "<p class=empty>No workspaces named. Ask for <code>?root=/path/to/repo</code>, once per repository.</p>".into();
    }
    // One highlight budget per response, split between the repos and the
    // group tour, so a rig with many repos is no slower than one big diff.
    let share = HIGHLIGHT_BUDGET / (roots.len() + usize::from(req.group.is_some()));
    let rendered: Vec<(String, Summary)> = thread::scope(|s| {
        let handles: Vec<_> = roots
            .iter()
            .enumerate()
            .map(|(i, root)| {
                let start = req
                    .starts
                    .iter()
                    .find(|(r, _)| r == root)
                    .map(|(_, c)| c.as_str());
                let view = req
                    .views
                    .iter()
                    .find(|(v, _)| *v == i)
                    .map(|(_, spec)| spec.as_str());
                s.spawn(move || workspace(i, root, start, view, share))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    (
                        "<section class=repo><p class=error>rendering panicked</p></section>"
                            .into(),
                        Summary::default(),
                    )
                })
            })
            .collect()
    });
    let mut out = String::new();
    let mut budget = share;
    if let Some(group) = &req.group {
        group_tour(Path::new(group), &rendered, &mut budget, &mut out);
    }
    if roots.len() > 1 {
        // Several repos read as tabs, one shown at a time. The page picks
        // which; the server only suggests a default: the repo with a tour,
        // else the one with changes, else the first.
        let default = rendered
            .iter()
            .position(|(_, s)| s.tour)
            .or_else(|| rendered.iter().position(|(_, s)| s.files > 0))
            .unwrap_or(0);
        out.push_str("<div class=tabbed><nav class=tabs>");
        for (i, (root, (_, sum))) in roots.iter().zip(&rendered).enumerate() {
            let _ = write!(
                out,
                "<button type=button data-tab=\"repo-{i}\"{}>{}{}",
                if i == default { " data-default" } else { "" },
                if sum.tour {
                    "<span class=tour-dot title=\"has a tour\"></span>"
                } else {
                    ""
                },
                esc(&repo_name(Path::new(root))),
            );
            if sum.error {
                out.push_str("<span class=del> !</span>");
            } else if sum.files > 0 {
                let _ = write!(
                    out,
                    " <span class=stat><span class=add>+{}</span> <span class=del>−{}</span></span>",
                    sum.added, sum.removed
                );
            }
            out.push_str("</button>");
        }
        out.push_str("</nav>");
    }
    for (html, _) in &rendered {
        out.push_str(html);
    }
    if roots.len() > 1 {
        out.push_str("</div>");
    }
    let _ = write!(out, "<footer>rendered {}</footer>", chrono_now());
    out
}

/// The group's own tour, above the repos, quoting from any of them. Each
/// quote's path starts with its repo's directory under the group, which is
/// how the tour was written and how it reads.
fn group_tour(group: &Path, rendered: &[(String, Summary)], budget: &mut usize, out: &mut String) {
    let group = match group.canonicalize() {
        Ok(g) => g,
        Err(e) => {
            let _ = write!(
                out,
                "<p class=error>group {}: {}</p>",
                esc(&group.display().to_string()),
                esc(&e.to_string())
            );
            return;
        }
    };
    let store = match state::Store::load(&group, None) {
        Ok(store) => store,
        Err(e) => {
            let _ = write!(
                out,
                "<p class=error>could not read the group's tour: {}</p>",
                esc(&format!("{e:#}"))
            );
            return;
        }
    };
    if let Some(tour) = store.tour().filter(|t| !t.trim().is_empty()) {
        render_group_tour(&group, tour, rendered, budget, out);
    }
}

fn render_group_tour(
    group: &Path,
    tour: &str,
    rendered: &[(String, Summary)],
    budget: &mut usize,
    out: &mut String,
) {
    let repos: Vec<(usize, String)> = rendered
        .iter()
        .enumerate()
        .filter_map(|(i, (_, sum))| {
            let rel = sum.root.as_ref()?.strip_prefix(group).ok()?;
            Some((i, rel.to_string_lossy().into_owned()))
        })
        .collect();
    let label = if repos.len() > 1 {
        format!(
            "Tour across {}",
            repos
                .iter()
                .map(|(_, r)| r.as_str())
                .collect::<Vec<_>>()
                .join(" + ")
        )
    } else {
        "Tour".to_string()
    };
    out.push_str("<div class=group-tour>");
    render_tour(
        "tg",
        &label,
        tour,
        budget,
        out,
        &mut |spec, budget, html| {
            let hit = repos.iter().find_map(|(i, rel)| {
                let rest = spec.strip_prefix(rel.as_str())?.strip_prefix('/')?;
                Some((*i, rest))
            });
            match hit {
                Some((i, rest)) => quote_html(i, rest, spec, &rendered[i].1.diff, budget, html),
                None => {
                    let _ = write!(
                        html,
                        "<figure class=quote><figcaption>{}</figcaption><p class=dim>no repo here by that name; quote paths start with one of: {}</p></figure>",
                        esc(spec),
                        esc(&repos
                            .iter()
                            .map(|(_, r)| r.as_str())
                            .collect::<Vec<_>>()
                            .join(", "))
                    );
                }
            }
        },
    );
    out.push_str("</div>");
}

fn repo_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// What a tab needs to know about a workspace without reading its page.
#[derive(Default)]
struct Summary {
    /// The repo's canonical root and parsed diff, so a group tour can quote
    /// across every repo on the page without diffing any of them twice.
    root: Option<PathBuf>,
    diff: Vec<FileDiff>,
    files: usize,
    added: u32,
    removed: u32,
    tour: bool,
    error: bool,
}

fn workspace(
    index: usize,
    root: &str,
    start: Option<&str>,
    view: Option<&str>,
    budget: usize,
) -> (String, Summary) {
    let mut out = String::new();
    let _ = write!(out, "<section class=repo id=\"repo-{index}\">");
    let summary = match render_workspace(index, Path::new(root), start, view, budget, &mut out) {
        Ok(summary) => summary,
        Err(e) => {
            let _ = write!(
                out,
                "<header class=repo-head><h1>{}</h1></header><p class=error>{}</p>",
                esc(&repo_name(Path::new(root))),
                esc(&format!("{e:#}"))
            );
            Summary {
                error: true,
                ..Summary::default()
            }
        }
    };
    out.push_str("</section>");
    (out, summary)
}

fn render_workspace(
    index: usize,
    root: &Path,
    start: Option<&str>,
    view: Option<&str>,
    mut budget: usize,
    out: &mut String,
) -> Result<Summary> {
    let root = canonical_repo(root).ok_or_else(|| {
        anyhow!(
            "{} is not the root of a jj or git repository",
            root.display()
        )
    })?;
    let backend = detect_backend_from(&root)
        .ok_or_else(|| anyhow!("{} is not a repository", root.display()))?;
    let mut notices = Vec::new();
    let store = match state::Store::load(&root, None) {
        Ok(store) => Some(store),
        Err(e) => {
            notices.push(format!("could not read recto state: {e:#}"));
            None
        }
    };
    let pull_request = store.as_ref().and_then(|s| s.pull_request().cloned());
    let branch_base = view_base(backend.as_ref(), pull_request.as_ref())?;
    let raw = backend
        .unified_diff(&Scope::Range(branch_base.clone()), false)
        .context("diff failed")?;
    let mut files = parse_diff(&raw);
    let anchor = anchor(backend.as_ref(), &branch_base, start, files.is_empty());
    let base = anchor.base.clone();
    if base != branch_base {
        let raw = backend
            .unified_diff(&Scope::Range(base.clone()), false)
            .context("diff failed")?;
        files = parse_diff(&raw);
    }
    if let Some(note) = &anchor.note {
        notices.push(note.clone());
    }
    // The revision list spans the anchored range. It is where landed work
    // stays visible, so it isn't worth drawing for fewer than two commits.
    let commits = backend
        .base_commit(&base)
        .and_then(|from| backend.commits(&from, backend.head_rev()).ok())
        .unwrap_or_default();
    let narrowed = view.and_then(|v| commits.iter().find(|c| c.id == v));
    let shown = match narrowed {
        Some(c) => parse_diff(
            &backend
                .unified_diff(&Scope::Rev(c.id.clone()), false)
                .context("diff failed")?,
        ),
        None => Vec::new(),
    };
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let tour = store.as_ref().and_then(|s| s.tour().map(str::to_string));
    let pending_notes = store.as_ref().map(|s| s.notes().0.len()).unwrap_or(0);

    let _ = write!(
        out,
        "<header class=repo-head><h1>{}</h1><div class=meta>",
        esc(&repo_name(&root))
    );
    let _ = write!(
        out,
        "<span>{}</span><span>{} → {}</span>",
        backend.kind(),
        esc(&anchor
            .label
            .clone()
            .unwrap_or_else(|| backend.base_display(&base))),
        esc(&backend
            .workspace_revision()
            .map(|r| short_oid(&r).to_string())
            .unwrap_or_else(|_| "@".into())),
    );
    if files.is_empty() {
        out.push_str("<span>no changes</span>");
    } else {
        let _ = write!(
            out,
            "<span>{} file{}</span><span class=add>+{added}</span><span class=del>−{removed}</span>",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        );
        out.push_str("<button type=button class=expand-all>expand all</button>");
    }
    out.push_str("</div>");
    if let Some(pr) = &pull_request {
        let _ = write!(
            out,
            "<div class=pr><a href=\"{}\" target=_blank rel=noopener>{}#{}</a> {}</div>",
            esc(&pr.url),
            esc(&pr.repository),
            pr.number,
            esc(&pr.title)
        );
    }
    if pending_notes > 0 {
        let _ = write!(
            out,
            "<div class=notes>{pending_notes} private note{} waiting for the agent</div>",
            if pending_notes == 1 { "" } else { "s" }
        );
    }
    for n in &notices {
        let _ = write!(out, "<p class=error>{}</p>", esc(n));
    }
    out.push_str("</header>");
    if commits.len() > 1 {
        let head = backend.commit_id(backend.head_rev());
        render_revisions(
            index,
            &commits,
            narrowed.map(|c| c.id.as_str()),
            head.as_deref(),
            anchor.label.is_some(),
            (added, removed),
            out,
        );
    }
    // The saved focus, for the page to highlight and, when it changes, scroll
    // to. A span in a file that left the diff has nowhere to point.
    if let Some(focus) = store.as_ref().and_then(|s| s.focus())
        && let Some(file) = files.iter().find(|f| f.path == focus.path)
    {
        let _ = write!(
            out,
            "<i class=focus-marker hidden data-focus=\"{index}:{}:{}:{}\"></i>",
            file_key(&file.path),
            focus.start,
            focus.end
        );
    }

    let tour = tour.filter(|t| !t.trim().is_empty());
    if let Some(tour) = tour.as_deref() {
        render_tour(
            &format!("t{index}"),
            "Tour",
            tour,
            &mut budget,
            out,
            &mut |spec, budget, html| quote_html(index, spec, spec, &files, budget, html),
        );
    }
    if let Some(c) = narrowed {
        let _ = write!(
            out,
            "<div class=narrowed>Showing <code>{}</code> {} alone. <button type=button data-view=\"{index}:\">show all changes</button></div>",
            esc(&c.change),
            esc(&c.summary)
        );
        if shown.is_empty() {
            out.push_str("<p class=empty>This commit changes no files.</p>");
        } else {
            render_files(index, &shown, &mut budget, out);
        }
    } else if !files.is_empty() {
        render_files(index, &files, &mut budget, out);
    }
    Ok(Summary {
        root: Some(root),
        files: files.len(),
        diff: files,
        added,
        removed,
        tour: tour.is_some(),
        error: false,
    })
}

/// The base a page diffs from once a rig's starting commit is known, and how
/// the header names it.
pub(crate) struct Anchor {
    pub(crate) base: Base,
    pub(crate) label: Option<String>,
    pub(crate) note: Option<String>,
}

/// A rig that pushes straight to trunk has an empty branch point the moment
/// its work lands, and a branch that's merged reads the same. Anchoring at
/// the commit the rig started from keeps that work on the page. It only
/// applies when everything between the start and the branch point is your
/// own landed work: a stack's lower layer isn't on trunk, and once someone
/// else's commits are in between, the range would show their work as yours.
pub(crate) fn anchor(
    backend: &dyn Backend,
    branch_base: &Base,
    start: Option<&str>,
    nothing_shown: bool,
) -> Anchor {
    let plain = |note: Option<String>| Anchor {
        base: branch_base.clone(),
        label: None,
        note: note.filter(|_| nothing_shown),
    };
    let Some(start) = start else {
        return plain(None);
    };
    let Some(s) = backend.commit_id(start) else {
        return plain(Some(format!(
            "The commit this rig started from ({}) isn't in this repository.",
            short_oid(start)
        )));
    };
    let Some(b) = backend.base_commit(branch_base) else {
        return plain(None);
    };
    if s == b || !backend.is_ancestor(&s, &b) {
        return plain(None);
    }
    if !backend.is_ancestor(&s, backend.head_rev()) {
        return plain(Some(
            "This workspace no longer descends from the commit the rig started from.".into(),
        ));
    }
    let landed = backend.trunk().is_some_and(|t| backend.is_ancestor(&b, &t));
    if !landed {
        return plain(None);
    }
    match backend.commits(&s, &b) {
        Ok(between) if between.iter().all(|c| c.mine || c.empty) => Anchor {
            base: Base::Revision(s.clone()),
            label: Some(format!("rig start {}", short_oid(&s))),
            note: None,
        },
        Ok(_) => plain(Some(
            "Trunk has moved past where this rig started, with other people's commits in between, so this diff starts at the branch point.".into(),
        )),
        Err(_) => plain(None),
    }
}

/// The commits in the page's range, newest first. Picking one narrows the
/// file list to that commit; the first row goes back to the whole range.
fn render_revisions(
    index: usize,
    commits: &[Commit],
    narrowed: Option<&str>,
    head: Option<&str>,
    from_start: bool,
    (added, removed): (u32, u32),
    out: &mut String,
) {
    // The range's own totals, not the commits' sum: a line one commit adds
    // and a later one rewrites counts once in the range.
    let landed = commits.iter().filter(|c| c.landed).count();
    let _ = write!(
        out,
        "<details class=revs id=\"revs-{index}\"{}><summary>{} commits{}</summary><ol>",
        if narrowed.is_some() { " open" } else { "" },
        commits.len(),
        if landed == 0 {
            String::new()
        } else if landed == commits.len() {
            " · all landed".into()
        } else {
            format!(" · {landed} landed")
        }
    );
    let _ = write!(
        out,
        "<li><button type=button data-view=\"{index}:\"{}><span class=rev-id></span><span class=rev-sum>all changes {}</span>{}</button></li>",
        if narrowed.is_none() {
            " class=active"
        } else {
            ""
        },
        if from_start {
            "since the rig started"
        } else {
            "in this range"
        },
        rev_stat(added, removed),
    );
    for c in commits {
        let _ = write!(
            out,
            "<li><button type=button data-view=\"{index}:{}\"{}{}><span class=rev-id>{}</span><span class=rev-sum>{}</span>{}<span class=rev-when>{}</span>{}</button></li>",
            esc(&c.id),
            if narrowed == Some(c.id.as_str()) {
                " class=active"
            } else {
                ""
            },
            if c.mine { "" } else { " data-foreign" },
            esc(&c.change),
            match (c.summary.is_empty(), head == Some(c.id.as_str())) {
                (true, true) => "<i>working copy</i>".into(),
                (true, false) => "<i>no description</i>".into(),
                (false, true) => format!("{} <i>(working copy)</i>", esc(&c.summary)),
                (false, false) => esc(&c.summary),
            },
            if c.landed {
                "<span class=landed>landed</span>"
            } else {
                ""
            },
            esc(&c.when),
            rev_stat(c.added, c.removed),
        );
    }
    out.push_str("</ol></details>");
}

fn rev_stat(added: u32, removed: u32) -> String {
    format!(
        "<span class=rev-stat><span class=add>+{added}</span> <span class=del>−{removed}</span></span>"
    )
}

/// The base a page diffs from, by the TUI's own rule: an attached PR's branch
/// point when this workspace can resolve it, otherwise the default base.
fn view_base(backend: &dyn Backend, pull_request: Option<&link::PullRequest>) -> Result<Base> {
    pull_request
        .map(|pr| Base::branch_point(pr.base_oid.clone()))
        .filter(|b| backend.resolves(b))
        .or_else(|| backend.default_bases().into_iter().next())
        .ok_or_else(|| anyhow!("no diff base available"))
}

/// `recto focus` with no viewer listening. Focus is "look here now", and the
/// web view is a viewer too, so it's saved where serve reads it, held to the
/// rules a TUI applies: the file has to be in the diff, the review can't be
/// stale, and a span outside every hunk is kept but reported. Returns the
/// process exit code.
pub(crate) fn save_focus_offline(path: &str, start: Option<u32>, end: Option<u32>) -> i32 {
    let result = (|| -> Result<std::result::Result<Option<String>, String>> {
        let backend = crate::backend::detect_backend()?;
        let mut store = state::Store::load(backend.root(), None)?;
        if let Some(pr) = store.pull_request() {
            let revision = backend.workspace_revision()?;
            if !pr.head_oid.eq_ignore_ascii_case(&revision) {
                return Ok(Err(format!(
                    "stale review: attached head {} does not match workspace revision {revision}; refresh the review before focusing or annotating",
                    pr.head_oid
                )));
            }
        }
        let base = view_base(backend.as_ref(), store.pull_request())?;
        let files = parse_diff(&backend.unified_diff(&Scope::Range(base), false)?);
        let Some(file) = files.iter().find(|f| f.path == path) else {
            return Ok(Err(format!("not in current diff: {path}")));
        };
        let Some(start) = start else {
            // A TUI scrolls to the file; the web view can only point at a
            // span. Leave the saved focus alone rather than claim success.
            return Ok(Err(format!(
                "with no viewer running, focus needs a line span ({path}:LINE or {path}:START-END)"
            )));
        };
        let end = end.unwrap_or(start).max(start);
        store.set_focus(Some(&crate::app::FocusAnchor {
            path: path.to_string(),
            start,
            end,
        }));
        store.save()?;
        let shown = file
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|l| l.new)
            .any(|n| (start..=end).contains(&n));
        Ok(if shown {
            Ok(None)
        } else {
            Err(format!(
                "{path}:{start}-{end} not in current diff (outside any shown hunk)"
            ))
        })
    })();
    match result {
        Ok(Ok(_)) => {
            eprintln!("recto: no viewer running; saved the focus for the web view");
            0
        }
        Ok(Err(refusal)) => {
            eprintln!("recto: {refusal}");
            1
        }
        Err(e) => {
            eprintln!("recto: {e:#}");
            2
        }
    }
}

/// `recto clear` with no viewer listening: drop the saved focus and
/// annotations, leaving notes and the tour alone as a viewer does.
pub(crate) fn save_clear_offline() -> i32 {
    let result = (|| -> Result<()> {
        let backend = crate::backend::detect_backend()?;
        let mut store = state::Store::load(backend.root(), None)?;
        store.set_focus(None);
        store.set_annotations(&[]);
        store.save()
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("recto: {e:#}");
            2
        }
    }
}

// ---------------------------------------------------------------------------
// Unified diff model

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ctx,
    Add,
    Del,
    Meta,
}

struct DiffLine {
    kind: Kind,
    old: Option<u32>,
    new: Option<u32>,
    text: String,
}

struct Hunk {
    header: String,
    lines: Vec<DiffLine>,
}

struct FileDiff {
    path: String,
    status: char,
    binary: bool,
    hunks: Vec<Hunk>,
    added: u32,
    removed: u32,
}

fn parse_diff(raw: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut in_hunk = false;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            files.push(FileDiff {
                path,
                status: 'M',
                binary: false,
                hunks: Vec::new(),
                added: 0,
                removed: 0,
            });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            if let Some((o, n)) = crate::diff::parse_hunk_starts(line) {
                old_no = o;
                new_no = n;
            }
            file.hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            if line.starts_with("new file mode") {
                file.status = 'A';
            } else if line.starts_with("deleted file mode") {
                file.status = 'D';
            } else if line.starts_with("rename to ") {
                file.status = 'R';
            } else if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
                file.binary = true;
            } else if let Some(p) = line.strip_prefix("+++ ") {
                let p = diff_path(p);
                if let Some(p) = p.strip_prefix("b/") {
                    file.path = p.to_string();
                }
            } else if let Some(p) = line.strip_prefix("--- ")
                && file.status == 'D'
            {
                let p = diff_path(p);
                if let Some(p) = p.strip_prefix("a/") {
                    file.path = p.to_string();
                }
            }
            continue;
        }
        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (Kind::Add, &line[1..]),
            Some(b'-') => (Kind::Del, &line[1..]),
            Some(b' ') => (Kind::Ctx, &line[1..]),
            Some(b'\\') => (Kind::Meta, line),
            None => (Kind::Ctx, ""),
            _ => (Kind::Meta, line),
        };
        let (old, new) = match kind {
            Kind::Add => {
                file.added += 1;
                new_no += 1;
                (None, Some(new_no - 1))
            }
            Kind::Del => {
                file.removed += 1;
                old_no += 1;
                (Some(old_no - 1), None)
            }
            Kind::Ctx => {
                old_no += 1;
                new_no += 1;
                (Some(old_no - 1), Some(new_no - 1))
            }
            Kind::Meta => (None, None),
        };
        hunk.lines.push(DiffLine {
            kind,
            old,
            new,
            text: text.to_string(),
        });
    }
    files
}

// ---------------------------------------------------------------------------
// HTML pieces

/// A file's id on the page, from its path rather than its position, so state
/// the page keeps across refreshes (open files, the focused span) stays on
/// the same file when the diff gains or loses others. FNV-1a, 32 bits.
fn file_key(path: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for b in path.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

fn render_files(index: usize, files: &[FileDiff], budget: &mut usize, out: &mut String) {
    out.push_str("<div class=files>");
    let lines: usize = files
        .iter()
        .flat_map(|f| &f.hunks)
        .map(|h| h.lines.len() + 1)
        .sum();
    let open = if lines <= OPEN_DIFF_LINES {
        " open"
    } else {
        ""
    };
    for file in files {
        let key = file_key(&file.path);
        let _ = write!(
            out,
            "<details class=file id=\"f{index}-{key}\"{open}><summary><span class=\"st st-{}\">{}</span><span class=path>{}</span><span class=stat><span class=add>+{}</span> <span class=del>−{}</span>{}</span></summary>",
            file.status,
            file.status,
            esc(&file.path),
            file.added,
            file.removed,
            diffstat_bar(file.added, file.removed),
        );
        if file.binary {
            out.push_str("<p class=dim>binary file</p>");
        } else {
            out.push_str(DIFF_TABLE);
            let ext = ext_for_path(&file.path);
            for hunk in &file.hunks {
                let _ = write!(
                    out,
                    "<tr class=hunk><td colspan=3>{}</td></tr>",
                    esc(&hunk.header)
                );
                for line in &hunk.lines {
                    diff_row(index, &key, line, ext, budget, out);
                }
            }
            out.push_str("</table>");
        }
        out.push_str("</details>");
    }
    out.push_str("</div>");
}

fn diff_row(
    index: usize,
    key: &str,
    line: &DiffLine,
    ext: &str,
    budget: &mut usize,
    out: &mut String,
) {
    let (class, sig) = match line.kind {
        Kind::Add => ("add", "+"),
        Kind::Del => ("del", "−"),
        Kind::Ctx => ("ctx", " "),
        Kind::Meta => ("meta", " "),
    };
    let id = match (line.kind, line.new) {
        (Kind::Add | Kind::Ctx, Some(n)) => format!(" id=\"L{index}-{key}-{n}\""),
        _ => String::new(),
    };
    let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
    let _ = write!(
        out,
        "<tr class={class}{id}><td class=ln>{}</td><td class=ln>{}</td><td class=code><span class=sig>{sig}</span>",
        num(line.old),
        num(line.new),
    );
    code_html(&line.text, ext, line.kind != Kind::Meta, budget, out);
    out.push_str("</td></tr>");
}

/// Five blocks split between added and removed in proportion, the way GitHub
/// draws a file's weight, so a collapsed list still shows where the change is.
fn diffstat_bar(added: u32, removed: u32) -> String {
    const BLOCKS: u32 = 5;
    let total = added + removed;
    let (mut a, mut d) = (0, 0);
    if total > 0 {
        let filled = BLOCKS.min(total);
        a = ((added * filled) as f32 / total as f32).round() as u32;
        if added > 0 && a == 0 {
            a = 1;
        }
        d = filled - a.min(filled);
        if removed > 0 && d == 0 && a > 1 {
            a -= 1;
            d = 1;
        }
    }
    let mut out = String::from("<span class=bar>");
    for i in 0..BLOCKS {
        out.push_str(if i < a {
            "<i class=a></i>"
        } else if i < a + d {
            "<i class=d></i>"
        } else {
            "<i></i>"
        });
    }
    out.push_str("</span>");
    out
}

/// One line of source, syntax highlighted while the response's budget lasts.
fn code_html(text: &str, ext: &str, highlight: bool, budget: &mut usize, out: &mut String) {
    let text = expand_tabs(text, 4);
    if !highlight || *budget == 0 {
        out.push_str(&esc(&text));
        return;
    }
    *budget -= 1;
    for span in highlighter().line_spans(&text, ext) {
        let mut style = String::new();
        if let Some(Color::Rgb(r, g, b)) = span.style.fg {
            let _ = write!(style, "color:#{r:02x}{g:02x}{b:02x};");
        }
        if span.style.add_modifier.contains(Modifier::BOLD) {
            style.push_str("font-weight:600;");
        }
        if span.style.add_modifier.contains(Modifier::ITALIC) {
            style.push_str("font-style:italic;");
        }
        if style.is_empty() {
            out.push_str(&esc(&span.content));
        } else {
            let _ = write!(out, "<span style=\"{style}\">{}</span>", esc(&span.content));
        }
    }
}

/// The tour as prose, with each `recto PATH:SPAN` fence replaced by the lines
/// it names from the current diff, and an outline of its sections up top.
/// Resolves one `recto PATH:SPAN` fence into its quoted lines.
type QuoteFn<'a> = dyn FnMut(&str, &mut usize, &mut String) + 'a;

/// A tour as prose. `ids` prefixes its heading anchors so two tours on one
/// page don't collide, and `quote` resolves each fence against whichever
/// diffs the tour can see.
fn render_tour(
    ids: &str,
    label: &str,
    source: &str,
    budget: &mut usize,
    out: &mut String,
    quote: &mut QuoteFn,
) {
    let parser = Parser::new_ext(source, Options::all() - Options::ENABLE_SMART_PUNCTUATION);
    let mut events: Vec<Event> = Vec::new();
    let mut outline: Vec<(String, String)> = Vec::new();
    let mut heading: Option<(String, String)> = None;
    let mut quoting: Option<String> = None;
    for event in parser {
        if quoting.is_some() {
            if let Event::End(TagEnd::CodeBlock) = event {
                let spec = quoting.take().unwrap_or_default();
                let mut html = String::new();
                quote(spec.trim(), budget, &mut html);
                events.push(Event::Html(html.into()));
            }
            continue;
        }
        match event {
            Event::Start(Tag::CodeBlock(ref kind)) => {
                if let Some(spec) = markdown::quote_spec(kind) {
                    quoting = Some(spec);
                    continue;
                }
                events.push(event);
            }
            Event::Start(Tag::Heading {
                level: level @ (HeadingLevel::H1 | HeadingLevel::H2),
                classes,
                attrs,
                ..
            }) => {
                let id = format!(
                    "{ids}-{}",
                    outline.len() + usize::from(heading.is_some()) + 1
                );
                heading = Some((id.clone(), String::new()));
                events.push(Event::Start(Tag::Heading {
                    level,
                    id: Some(CowStr::from(id)),
                    classes,
                    attrs,
                }));
            }
            Event::End(TagEnd::Heading(level @ (HeadingLevel::H1 | HeadingLevel::H2))) => {
                if let Some(h) = heading.take() {
                    outline.push(h);
                }
                events.push(Event::End(TagEnd::Heading(level)));
            }
            Event::Text(ref t) | Event::Code(ref t) => {
                if let Some((_, title)) = heading.as_mut() {
                    title.push_str(t);
                }
                events.push(event);
            }
            // Tours are agent-written and may quote text from anywhere; show
            // any raw HTML as the text it is rather than letting it run.
            Event::Html(t) | Event::InlineHtml(t) => events.push(Event::Text(t)),
            // A link keeps its text, but only an http(s), mailto or in-page
            // target. An image never loads (that would fetch on open); it
            // becomes a link to itself.
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => events.push(Event::Start(Tag::Link {
                link_type,
                dest_url: safe_url(dest_url),
                title,
                id,
            })),
            Event::Start(Tag::Image {
                dest_url,
                title,
                id,
                ..
            }) => {
                events.push(Event::Start(Tag::Link {
                    link_type: pulldown_cmark::LinkType::Inline,
                    dest_url: safe_url(dest_url),
                    title,
                    id,
                }));
                events.push(Event::Text("image: ".into()));
            }
            Event::End(TagEnd::Image) => events.push(Event::End(TagEnd::Link)),
            other => events.push(other),
        }
    }
    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, events.into_iter());

    let _ = write!(
        out,
        "<article class=tour><div class=tour-label>{}</div>",
        esc(label)
    );
    if outline.len() > 1 {
        // Headings that carry their own number ("1. Setup") would read twice
        // under the list's counter, and an unnumbered intro beside them reads
        // fine without one.
        let numbered = outline.iter().any(|(_, t)| {
            let t = t.trim_start();
            let digits = t.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && matches!(t[digits..].chars().next(), Some('.' | ')' | ':'))
        });
        out.push_str(if numbered {
            "<ol class=\"outline plain\">"
        } else {
            "<ol class=outline>"
        });
        for (id, title) in &outline {
            let _ = write!(out, "<li><a href=\"#{id}\">{}</a></li>", esc(title));
        }
        out.push_str("</ol>");
    }
    out.push_str(&body);
    out.push_str("</article>");
}

/// Quote `spec`'s lines from `files`, the diff of the repo at `index`.
/// `caption` is how the tour wrote the spec, which a group tour prefixes with
/// the repo's directory.
fn quote_html(
    index: usize,
    spec: &str,
    caption: &str,
    files: &[FileDiff],
    budget: &mut usize,
    out: &mut String,
) {
    let (path, start, end) = parse_pathspec(spec);
    let found = files.iter().position(|f| f.path == path);
    let anchor = match (found, start) {
        (Some(fi), Some(s)) => {
            let key = file_key(&files[fi].path);
            format!(
                "#L{index}-{key}-{s}\" data-focus=\"{index}:{key}:{s}:{}",
                end.unwrap_or(s).max(s)
            )
        }
        (Some(fi), None) => format!("#f{index}-{}", file_key(&files[fi].path)),
        _ => String::new(),
    };
    out.push_str("<figure class=quote><figcaption>");
    if anchor.is_empty() {
        out.push_str(&esc(caption));
    } else {
        let _ = write!(out, "<a href=\"{anchor}\">{}</a>", esc(caption));
    }
    out.push_str("</figcaption>");
    let rows = found.and_then(|fi| {
        let start = start?;
        let end = end.unwrap_or(start).max(start);
        let file = &files[fi];
        let mut picked: Vec<&DiffLine> = Vec::new();
        for hunk in &file.hunks {
            // A deletion sits just before the next surviving line, so it
            // belongs to the span when that line is strictly inside it.
            let mut next_new = hunk.lines.iter().find_map(|l| l.new).unwrap_or(0);
            for line in &hunk.lines {
                let take = match line.kind {
                    Kind::Add | Kind::Ctx => line.new.is_some_and(|n| (start..=end).contains(&n)),
                    Kind::Del => next_new > start && next_new <= end,
                    Kind::Meta => false,
                };
                if let Some(n) = line.new {
                    next_new = n + 1;
                }
                if take {
                    picked.push(line);
                }
            }
        }
        (!picked.is_empty()).then_some((fi, picked))
    });
    match rows {
        Some((fi, picked)) => {
            let ext = ext_for_path(&files[fi].path);
            out.push_str(DIFF_TABLE);
            for line in picked {
                diff_row(index, &file_key(&files[fi].path), line, ext, budget, out);
            }
            out.push_str("</table>");
        }
        // Tours outlive the diff they were written against; say so in place.
        None => out.push_str("<p class=dim>not in this diff</p>"),
    }
    out.push_str("</figure>");
}

// ---------------------------------------------------------------------------
// Small helpers

/// `url` when it's safe to put in a tour's page, else an inert `#`.
fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
    let lower = url.trim().to_ascii_lowercase();
    let scheme = lower.split_once(':').map(|(s, _)| s);
    match scheme {
        None => url,
        Some(s) if s.contains('/') || s.contains('#') || s.contains('?') => url,
        Some("http" | "https" | "mailto") => url,
        Some(_) => CowStr::from("#"),
    }
}

/// A path as git writes it in a `+++`/`---` line: C-quoted when it holds
/// unusual bytes, and followed by a tab when it holds a space.
fn diff_path(raw: &str) -> String {
    let raw = raw.trim_end_matches(['\t', '\r']);
    let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else {
        return raw.to_string();
    };
    let mut out = Vec::new();
    let mut bytes = inner.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(d @ b'0'..=b'7') => {
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    if let Some(&n @ b'0'..=b'7') = bytes.peek() {
                        v = v * 8 + u32::from(n - b'0');
                        bytes.next();
                    }
                }
                out.push(v as u8);
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Local wall-clock time for the footer, without pulling in a date crate.
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes only into the tm we own.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Column widths are declared up front: a fixed-layout table otherwise sizes
/// its columns from the first row, which is a hunk header spanning all three,
/// and splits the width evenly between two line numbers and the code.
const DIFF_TABLE: &str = "<table class=diff><colgroup><col class=ln><col class=ln><col></colgroup>";

const CSS: &str = include_str!("serve/page.css");
const JS: &str = include_str!("serve/page.js");

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,4 +1,4 @@\n fn a() {\n-    old();\n+    new();\n     keep();\n }\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,1 @@\n+hello\n";

    #[test]
    fn parses_files_hunks_and_line_numbers() {
        let files = parse_diff(DIFF);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!((files[0].added, files[0].removed), (1, 1));
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines[1].old, Some(2));
        assert_eq!(lines[2].new, Some(2));
        assert_eq!(lines[3].new, Some(3));
        assert_eq!(files[1].status, 'A');
        assert_eq!(files[1].hunks[0].lines[0].new, Some(1));
    }

    #[test]
    fn quote_takes_the_span_and_the_deletion_inside_it() {
        let files = parse_diff(DIFF);
        let mut budget = 0;
        let mut out = String::new();
        quote_html(
            0,
            "src/a.rs:1-3",
            "src/a.rs:1-3",
            &files,
            &mut budget,
            &mut out,
        );
        assert!(out.contains("old();"), "{out}");
        assert!(out.contains("new();"), "{out}");
        assert!(out.contains("keep();"), "{out}");
        let key = file_key("src/a.rs");
        assert!(out.contains(&format!("href=\"#L0-{key}-1\"")), "{out}");
    }

    #[test]
    fn quote_outside_the_diff_says_so() {
        let files = parse_diff(DIFF);
        let mut out = String::new();
        quote_html(
            0,
            "src/gone.rs:4",
            "src/gone.rs:4",
            &files,
            &mut 0,
            &mut out,
        );
        assert!(out.contains("not in this diff"));
    }

    #[test]
    fn tour_html_is_escaped_and_outlined() {
        let files = parse_diff(DIFF);
        let mut out = String::new();
        render_tour(
            "t0",
            "Tour",
            "# One\n\n<script>x</script>\n\n## Two\n\n```recto src/a.rs:2\n```\n",
            &mut 0,
            &mut out,
            &mut |spec, budget, html| quote_html(0, spec, spec, &files, budget, html),
        );
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("href=\"#t0-1\""), "{out}");
        assert!(out.contains("id=\"t0-2\""), "{out}");
        assert!(out.contains("new();"), "{out}");
    }

    #[test]
    fn group_tour_quotes_each_repo_by_its_directory() {
        let group = Path::new("/w/rig");
        let summary = |dir: &str| Summary {
            root: Some(group.join(dir)),
            diff: parse_diff(DIFF),
            ..Summary::default()
        };
        let rendered = vec![
            (String::new(), summary("alpha")),
            (String::new(), summary("beta")),
        ];
        let mut out = String::new();
        render_group_tour(
            group,
            "# Across\n\n```recto beta/src/a.rs:2\n```\n\n```recto gamma/x.rs:1\n```\n",
            &rendered,
            &mut 0,
            &mut out,
        );
        assert!(out.contains("Tour across alpha + beta"), "{out}");
        // The quote resolves in the second repo and links into its section.
        assert!(
            out.contains(&format!(
                "href=\"#L1-{k}-2\" data-focus=\"1:{k}:2:2\">beta/src/a.rs:2</a>",
                k = file_key("src/a.rs")
            )),
            "{out}"
        );
        assert!(out.contains("new();"), "{out}");
        assert!(out.contains("no repo here by that name"), "{out}");
        assert!(out.contains("id=\"tg-1\""), "{out}");
    }

    #[test]
    fn tour_links_and_images_cant_run_or_fetch() {
        let mut out = String::new();
        render_tour(
            "t0",
            "Tour",
            "[x](javascript:alert(1)) [ok](https://example.com) [rel](#t0-1) ![pic](https://tracker/p.png)",
            &mut 0,
            &mut out,
            &mut |_, _, _| {},
        );
        assert!(!out.contains("javascript:"), "{out}");
        assert!(out.contains("href=\"https://example.com\""), "{out}");
        assert!(out.contains("href=\"#t0-1\""), "{out}");
        assert!(!out.contains("<img"), "{out}");
        assert!(out.contains("image: pic"), "{out}");
    }

    #[test]
    fn diff_paths_with_spaces_and_quotes() {
        let diff = "diff --git a/my notes.md b/my notes.md\n--- a/my notes.md\t\n+++ b/my notes.md\t\n@@ -1 +1 @@\n-a\n+b\ndiff --git \"a/caf\\303\\251\" \"b/caf\\303\\251\"\n--- \"a/caf\\303\\251\"\n+++ \"b/caf\\303\\251\"\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_diff(diff);
        assert_eq!(files[0].path, "my notes.md");
        assert_eq!(files[1].path, "café");
    }

    #[test]
    fn decodes_query_values() {
        assert_eq!(percent_decode("%2Fhome%2Fp+q"), "/home/p q");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
