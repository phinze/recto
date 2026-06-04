//! Agent link: a Unix-socket control channel so companion sessions can drive
//! the running TUI (focus a span, ping for liveness). Discovery is by
//! workspace root, so `recto focus …` run anywhere inside the repo reaches
//! the recto reviewing it, with no env var to thread through to the agent.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// A command from a companion session. JSON-tagged on the wire so the
/// vocabulary can grow (tour manifests, etc.) without breaking older clients.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Scroll to and highlight a span. `start`/`end` are new-side (post-image)
    /// line numbers; both absent means "whole file".
    Focus {
        path: String,
        start: Option<u32>,
        end: Option<u32>,
    },
    /// Clear any active focus highlight.
    Clear,
    /// Liveness check.
    Ping,
}

/// Reply to a [`Request`]. `error` carries the reason when `ok` is false, so a
/// driving agent can learn (e.g.) that a target wasn't in the current diff.
/// `note` carries an informational aside on success — e.g. that recto was in an
/// editor and drove neovim directly rather than scrolling the TUI.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            note: None,
        }
    }

    pub fn ok_note(msg: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            note: Some(msg.into()),
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            note: None,
        }
    }
}

/// A request handed to the main loop, paired with a one-shot channel the loop
/// uses to send its [`Response`] back to the waiting connection.
pub struct Incoming {
    pub request: Request,
    pub respond: mpsc::Sender<Response>,
}

/// Live handle to a neovim instance recto launched via the `e` keybind. While
/// it's up, the main loop is parked in the editor handoff, so the listener
/// thread drives the editor directly over this RPC address instead.
pub struct NvimHandle {
    /// The editor program (e.g. `vim` or `nvim`); also our `--remote-expr` client.
    pub prog: String,
    /// The `--listen` socket we handed neovim, our handle to drive it.
    pub addr: PathBuf,
}

/// Whether recto is suspended in an editor, and if so whether that editor is a
/// drivable neovim. The main loop sets this around the editor handoff; the
/// listener thread reads it to decide whether to answer requests itself rather
/// than queue them for a loop that can't tick until the editor exits.
#[derive(Default)]
pub struct EditorLink {
    active: AtomicBool,
    nvim: Mutex<Option<NvimHandle>>,
}

impl EditorLink {
    /// Mark recto as entering an editor handoff, recording the neovim handle if
    /// the editor is drivable.
    pub fn enter(&self, nvim: Option<NvimHandle>) {
        *self.nvim.lock().unwrap() = nvim;
        self.active.store(true, Ordering::SeqCst);
    }

    /// Mark the editor handoff as finished; the main loop is ticking again.
    pub fn leave(&self) {
        self.active.store(false, Ordering::SeqCst);
        *self.nvim.lock().unwrap() = None;
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

/// Walk up from `start` looking for `.jj/` (preferred) then `.git/`, mirroring
/// `detect_backend`. The directory we land on is the workspace identity both
/// server and client hash to agree on a socket path.
pub fn workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".jj").is_dir() || d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Socket path for a workspace root. A readable basename aids debugging; the
/// hash of the canonical path keeps distinct repos (and jj workspaces) apart.
pub fn socket_path(root: &Path) -> PathBuf {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canon.hash(&mut hasher);
    let hash = hasher.finish();
    let base = canon
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("recto");
    runtime_dir().join(format!("{base}-{hash:016x}.sock"))
}

/// RPC socket address to hand neovim via `--listen`, so the listener can drive
/// it. Keyed by recto's pid so concurrent instances don't fight over one
/// address; lives beside the control socket in the same hardened dir.
pub fn nvim_addr(pid: u32) -> PathBuf {
    runtime_dir().join(format!("nvim-{pid}.sock"))
}

/// `$XDG_RUNTIME_DIR/recto` (0700 by virtue of its parent), falling back to a
/// `recto-<uid>` dir under the system temp dir when the runtime dir is unset.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("recto")
}

/// Resolve the socket path for the current working directory. `RECTO_SOCK`
/// overrides discovery entirely.
pub fn socket_for_cwd() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("RECTO_SOCK") {
        return Ok(PathBuf::from(p));
    }
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let root = workspace_root(&cwd).ok_or_else(|| {
        anyhow!(
            "not inside a jj or git repository (looked from {})",
            cwd.display()
        )
    })?;
    Ok(socket_path(&root))
}

/// Bind the listener and spawn its accept thread. Returns a receiver the main
/// loop drains each tick. Last-one-wins: a stale or live socket at `path` is
/// unlinked and rebound, so the newest recto owns the workspace.
pub fn spawn_listener(path: &Path, editor: Arc<EditorLink>) -> Result<mpsc::Receiver<Incoming>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
        harden_dir(parent);
    }
    if path.exists() {
        std::fs::remove_file(path).ok();
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;
    let (tx, rx) = mpsc::channel::<Incoming>();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            handle_conn(stream, &tx, &editor);
        }
    });
    Ok(rx)
}

/// Read one request line, hand it to the main loop, and write back the reply.
/// Connections are served serially; commands are rare enough that this is fine.
fn handle_conn(stream: UnixStream, tx: &mpsc::Sender<Incoming>, editor: &EditorLink) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            write_response(&mut writer, &Response::err(format!("bad request: {e}")));
            return;
        }
    };

    // While recto is suspended in an editor the main loop can't answer, so we
    // reply on this thread instead of waiting it out. A live neovim gets driven
    // directly (cursor + highlight where the user's eyes already are); we still
    // queue the focus so recto's own sticky highlight is set for the return.
    if editor.is_active() {
        let resp = handle_while_in_editor(&request, tx, editor);
        write_response(&mut writer, &resp);
        return;
    }

    let (rtx, rrx) = mpsc::channel::<Response>();
    if tx
        .send(Incoming {
            request,
            respond: rtx,
        })
        .is_err()
    {
        return;
    }
    // The main loop drains commands each ~50ms tick; bound the wait so a client
    // never hangs forever if the loop is wedged for some unforeseen reason.
    let resp = rrx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| Response::err("recto did not respond in time"));
    write_response(&mut writer, &resp);
}

/// Answer a request that arrived while recto is parked in an editor handoff.
/// Drives neovim when we can; always queues the request so the main loop
/// applies it (sets the sticky focus) once it resumes.
fn handle_while_in_editor(
    request: &Request,
    tx: &mpsc::Sender<Incoming>,
    editor: &EditorLink,
) -> Response {
    let nvim = editor.nvim.lock().unwrap();
    match request {
        Request::Ping => Response::ok_note("recto is in an editor"),
        Request::Clear => {
            if let Some(h) = nvim.as_ref() {
                drive_nvim_clear(h);
            }
            queue(tx, request.clone());
            Response::ok()
        }
        Request::Focus { path, start, end } => {
            let drove = nvim
                .as_ref()
                .map(|h| drive_nvim_focus(h, path, *start, *end))
                .unwrap_or(false);
            queue(tx, request.clone());
            if drove {
                Response::ok_note("recto is in neovim; drove the editor there")
            } else {
                Response::ok_note("recto is in an editor; focus will apply when you return")
            }
        }
    }
}

/// Hand a request to the main loop with a throwaway response channel. Used from
/// the editor fast-path, where we've already replied on this thread — the loop
/// applies the request on resume and its response simply goes nowhere.
fn queue(tx: &mpsc::Sender<Incoming>, request: Request) {
    let (respond, _) = mpsc::channel::<Response>();
    let _ = tx.send(Incoming { request, respond });
}

/// Move the running neovim's cursor to a span and highlight the range. Prefers
/// the `RectoFocus` Lua helper (shipped in nixvim-config) for a real range
/// highlight, falling back to a plain edit + center over core Ex commands when
/// the helper isn't loaded — so this works before nixvim-config catches up.
fn drive_nvim_focus(h: &NvimHandle, path: &str, start: Option<u32>, end: Option<u32>) -> bool {
    // The wire path is workspace-root-relative; neovim's cwd may be a subdir,
    // so resolve to absolute before asking it to `:edit`.
    let p = vim_squote(&abs_path(path));
    let (s, e) = match start {
        Some(s) => (s.to_string(), end.unwrap_or(s).max(s).to_string()),
        None => ("v:null".into(), "v:null".into()),
    };
    if remote_expr(h, &format!("v:lua.RectoFocus('{p}', {s}, {e})")) {
        return true;
    }
    let fallback = match start {
        Some(s) => {
            format!("execute('edit '.fnameescape('{p}').' | call cursor({s},1) | normal! zz')")
        }
        None => format!("execute('edit '.fnameescape('{p}'))"),
    };
    remote_expr(h, &fallback)
}

/// Clear neovim's focus highlight via the `RectoClear` helper. A no-op (returns
/// false) if the helper isn't loaded; there's no core-Ex fallback worth the
/// noise, since the highlight only exists when the helper set it.
fn drive_nvim_clear(h: &NvimHandle) -> bool {
    remote_expr(h, "v:lua.RectoClear()")
}

/// Evaluate a Vimscript expression in the running neovim over its RPC socket.
/// Returns whether the client exited cleanly — false means the editor was gone
/// or the expression errored (e.g. the helper function isn't defined).
fn remote_expr(h: &NvimHandle, expr: &str) -> bool {
    Command::new(&h.prog)
        .arg("--server")
        .arg(&h.addr)
        .arg("--remote-expr")
        .arg(expr)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve a workspace-root-relative path to absolute, against the workspace
/// root recto is reviewing (not its cwd, which may differ from neovim's).
/// Falls back to the input unchanged if discovery fails.
fn abs_path(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    if let Ok(cwd) = std::env::current_dir() {
        let base = workspace_root(&cwd).unwrap_or(cwd);
        return base.join(p).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Escape a path for embedding in a single-quoted Vimscript string literal:
/// double any single quotes. `fnameescape` (applied in the expression itself)
/// then handles spaces and other special characters.
fn vim_squote(s: &str) -> String {
    s.replace('\'', "''")
}

fn write_response(writer: &mut UnixStream, resp: &Response) {
    if let Ok(body) = serde_json::to_string(resp) {
        let _ = writeln!(writer, "{body}");
    }
}

/// Connect to a running recto and send one request, returning its response.
pub fn send(path: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("no recto listening for this workspace ({})", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let body = serde_json::to_string(request)?;
    stream.write_all(body.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader
        .read_line(&mut resp_line)
        .context("reading response from recto")?;
    serde_json::from_str(resp_line.trim()).context("malformed response from recto")
}

#[cfg(unix)]
fn harden_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind a listener, drain one request on a worker thread, and confirm the
    /// client `send` gets the worker's reply back — the full socket round trip.
    #[test]
    fn round_trip_focus() {
        let path = std::env::temp_dir().join(format!("recto-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let rx = spawn_listener(&path, Arc::new(EditorLink::default())).expect("bind");

        let worker = thread::spawn(move || {
            let incoming = rx.recv().expect("recv request");
            match &incoming.request {
                Request::Focus { path, start, end } => {
                    assert_eq!(path, "src/main.rs");
                    assert_eq!(*start, Some(12));
                    assert_eq!(*end, Some(20));
                }
                other => panic!("unexpected request: {other:?}"),
            }
            incoming.respond.send(Response::ok()).expect("respond");
        });

        let resp = send(
            &path,
            &Request::Focus {
                path: "src/main.rs".into(),
                start: Some(12),
                end: Some(20),
            },
        )
        .expect("send");
        assert!(resp.ok, "expected ok response, got {resp:?}");

        worker.join().expect("worker");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_request_is_refused() {
        let path = std::env::temp_dir().join(format!("recto-bad-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _rx = spawn_listener(&path, Arc::new(EditorLink::default())).expect("bind");

        let mut stream = UnixStream::connect(&path).expect("connect");
        stream.write_all(b"not json\n").expect("write");
        stream.flush().ok();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(!resp.ok);
        assert!(resp.error.is_some());
        let _ = std::fs::remove_file(&path);
    }
}
