//! Agent link: a Unix-socket control channel so companion sessions can drive
//! the running TUI (focus a span, ping for liveness). Discovery is by
//! workspace root, so `recto focus …` run anywhere inside the repo reaches
//! the recto reviewing it, with no env var to thread through to the agent.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
        }
    }
}

/// A request handed to the main loop, paired with a one-shot channel the loop
/// uses to send its [`Response`] back to the waiting connection.
pub struct Incoming {
    pub request: Request,
    pub respond: mpsc::Sender<Response>,
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
pub fn spawn_listener(path: &Path) -> Result<mpsc::Receiver<Incoming>> {
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
            handle_conn(stream, &tx);
        }
    });
    Ok(rx)
}

/// Read one request line, hand it to the main loop, and write back the reply.
/// Connections are served serially; commands are rare enough that this is fine.
fn handle_conn(stream: UnixStream, tx: &mpsc::Sender<Incoming>) {
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
    // The main loop drains commands each ~50ms tick, except while blocked in
    // an editor handoff — bound the wait so a client never hangs forever.
    let resp = rrx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| Response::err("recto did not respond in time"));
    write_response(&mut writer, &resp);
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
        let rx = spawn_listener(&path).expect("bind");

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
        let _rx = spawn_listener(&path).expect("bind");

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
