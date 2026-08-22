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

pub use crate::backend::repository_root as workspace_root;

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
    /// Replace the annotation set: labeled spans rendered as numbered inline
    /// notes ("step 1 here, step 2 there"). An empty set clears them.
    Annotate { sites: Vec<Site> },
    /// Clear any active focus highlight and annotations.
    Clear,
    /// Liveness check.
    Ping,
    /// Attach a read-only GitHub pull request snapshot to the review surface.
    /// Fetching happens in the client process, so the running TUI remains
    /// network-agnostic and startup stays offline.
    #[serde(rename = "pr")]
    AttachPr { pull_request: Box<PullRequest> },
    /// Pin a private note for the local agent to a span, appended to the pending set.
    /// `start`/`end` are new-side line numbers like `Focus`. Unlike `Annotate`
    /// (one tour, replaced wholesale) notes accumulate: each call adds one.
    #[serde(rename = "note", alias = "comment")]
    AgentNote {
        path: String,
        start: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        end: Option<u32>,
        body: String,
    },
    /// Drain the pending agent notes: hand them over and clear the set.
    /// Delivered means gone, so a drain that loses its reply loses the notes —
    /// which is why this is never queued behind an editor handoff.
    #[serde(rename = "notes", alias = "comments")]
    AgentNotes,
    /// Read the shared, local-only review draft without consuming it.
    #[serde(rename = "review")]
    ReviewDraft,
    /// Create, revise, or delete one shared inline review comment. New comments
    /// carry an anchor and no id; revisions carry the stable id. An empty body
    /// deletes an existing draft.
    #[serde(rename = "review-comment")]
    ReviewDraftComment {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        start: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        end: Option<u32>,
        body: String,
    },
}

/// One labeled site in an [`Request::Annotate`] set. `start`/`end` are
/// new-side line numbers like `Focus`; `end` defaults to `start`. Step
/// numbers are implicit from order.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Site {
    pub path: String,
    pub start: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<u32>,
    pub label: String,
}

/// One private note handed back by a [`Request::AgentNotes`] drain. Carries
/// the anchoring snippet alongside the body: the agent starts editing the
/// moment it reads this, so `path:line` goes stale almost immediately while the
/// quoted text stays meaningful.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentNote {
    /// 1-based position in the drained set, matching the on-screen badge.
    pub n: usize,
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub body: String,
    /// The diff rows around the span, absent when the span no longer resolves
    /// against the diff recto is currently showing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Vec<SnippetRow>>,
}

/// The session-durable, local-only public review being co-authored in recto. Reading
/// this object never changes it; posting is deliberately a later boundary.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewDraft {
    pub pull_request: PullRequestRef,
    pub comments: Vec<DraftReviewComment>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DraftReviewComment {
    pub id: u64,
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub body: String,
    pub last_editor: DraftEditor,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DraftEditor {
    User,
    Agent,
}

/// Read-only GitHub pull request context. These are public review objects, not
/// the private [`AgentNote`] channel, even when their body/author shapes rhyme.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PullRequest {
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: Actor,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    pub url: String,
    pub conversation: Vec<ConversationComment>,
    pub reviews: Vec<ReviewSummary>,
    #[serde(default)]
    pub threads: Vec<ReviewThread>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Actor {
    pub login: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConversationComment {
    pub author: Actor,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewSummary {
    pub author: Actor,
    pub body: String,
    pub state: ReviewState,
    pub submitted_at: Option<String>,
    pub commit_oid: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    pub side: DiffSide,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub resolved: bool,
    pub outdated: bool,
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewComment {
    pub id: String,
    pub database_id: Option<u64>,
    pub author: Actor,
    pub body: String,
    pub created_at: String,
    pub url: String,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    Left,
    Right,
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
    Unknown,
}

/// One row of a note's snippet, mirroring what the reviewer had on screen:
/// the new-side line number (absent on removed lines), the diff sign, and the
/// body text. `commented` marks the rows the comment actually points at, as
/// opposed to the surrounding context.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SnippetRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub sign: char,
    pub text: String,
    pub commented: bool,
}

/// Reply to a [`Request`]. `error` carries the reason when `ok` is false, so a
/// driving agent can learn (e.g.) that a target wasn't in the current diff.
/// `note` carries an informational aside on success — e.g. that recto was in an
/// editor and drove neovim directly rather than scrolling the TUI. `status` is
/// the machine-readable snapshot a [`Request::Ping`] asks for; absent otherwise.
/// `comments` is the legacy wire field carrying a drained [`Request::AgentNotes`]
/// set. The field name stays stable for older companion clients.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comments: Option<Vec<AgentNote>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_draft: Option<ReviewDraft>,
}

/// What a [`Request::Ping`] reports back: recto's identity, current diff, and
/// the active presentation surface's command capabilities. `files` remains the
/// changed-path list for compatibility; `capabilities` says whether that list
/// actually bounds a command on the current surface.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Status {
    /// recto's version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The running recto's pid.
    pub pid: u32,
    /// Which VCS the backend speaks: `"jj"` or `"git"`.
    pub backend: String,
    /// Absolute workspace root recto is reviewing.
    pub workspace_root: String,
    /// The base label shown in the header (e.g. `@-`, `trunk()`, `HEAD`).
    pub base: String,
    /// `"range"` for the whole base diff, or `"rev"` when narrowed to one rev.
    pub scope: String,
    /// Changed paths in the current diff. On the recto surface these bound both
    /// commands; a live neovim can focus any workspace path instead.
    pub files: Vec<String>,
    /// Where commands will present themselves right now.
    #[serde(default)]
    pub surface: Surface,
    /// How `focus` and `annotate` behave on that surface.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Whether a companion focus highlight is currently active.
    pub focus: bool,
    /// Number of active tour annotations.
    pub annotations: usize,
    /// Private agent notes waiting for a `recto notes` drain. The field retains
    /// its old wire name so older companion clients can still discover them.
    #[serde(default)]
    pub pending_comments: usize,
    /// Durable public review comments currently being co-authored locally.
    #[serde(default)]
    pub draft_comments: usize,
    /// Public PR snapshot currently attached to the TUI, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestRef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PullRequestRef {
    pub repository: String,
    pub number: u64,
    pub head_oid: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    #[default]
    Recto,
    Neovim,
    Editor,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct Capabilities {
    pub focus: Capability,
    pub annotate: Capability,
}

impl Capabilities {
    pub fn recto() -> Self {
        Self {
            focus: Capability::live(TargetScope::CurrentDiff),
            annotate: Capability::live(TargetScope::CurrentDiff),
        }
    }

    fn neovim() -> Self {
        Self {
            focus: Capability::live(TargetScope::Workspace),
            annotate: Capability::deferred(TargetScope::CurrentDiff),
        }
    }

    fn editor() -> Self {
        Self {
            focus: Capability::deferred(TargetScope::CurrentDiff),
            annotate: Capability::deferred(TargetScope::CurrentDiff),
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::recto()
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct Capability {
    pub delivery: Delivery,
    pub scope: TargetScope,
}

impl Capability {
    fn live(scope: TargetScope) -> Self {
        Self {
            delivery: Delivery::Live,
            scope,
        }
    }

    fn deferred(scope: TargetScope) -> Self {
        Self {
            delivery: Delivery::Deferred,
            scope,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Live,
    Deferred,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetScope {
    CurrentDiff,
    Workspace,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            note: None,
            status: None,
            comments: None,
            review_draft: None,
        }
    }

    pub fn ok_note(msg: impl Into<String>) -> Self {
        Self {
            note: Some(msg.into()),
            ..Self::ok()
        }
    }

    pub fn ok_status(status: Status) -> Self {
        Self {
            status: Some(status),
            ..Self::ok()
        }
    }

    pub fn ok_agent_notes(comments: Vec<AgentNote>) -> Self {
        Self {
            comments: Some(comments),
            ..Self::ok()
        }
    }

    pub fn ok_review_draft(review_draft: ReviewDraft) -> Self {
        Self {
            review_draft: Some(review_draft),
            ..Self::ok()
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            ..Self::ok()
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
    /// Status snapshot captured at editor entry, so the listener thread can
    /// answer a `ping` while the main loop is parked. The diff can't change
    /// while the loop is blocked in the editor, so the snapshot stays accurate.
    status: Mutex<Option<Status>>,
}

impl EditorLink {
    /// Mark recto as entering an editor handoff, recording the neovim handle if
    /// the editor is drivable and a status snapshot to serve while parked.
    pub fn enter(&self, nvim: Option<NvimHandle>, mut status: Status) {
        if nvim.is_some() {
            status.surface = Surface::Neovim;
            status.capabilities = Capabilities::neovim();
        } else {
            status.surface = Surface::Editor;
            status.capabilities = Capabilities::editor();
        }
        *self.nvim.lock().unwrap() = nvim;
        *self.status.lock().unwrap() = Some(status);
        self.active.store(true, Ordering::SeqCst);
    }

    /// Mark the editor handoff as finished; the main loop is ticking again.
    pub fn leave(&self) {
        self.active.store(false, Ordering::SeqCst);
        *self.nvim.lock().unwrap() = None;
        *self.status.lock().unwrap() = None;
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
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
        Request::Ping => {
            let mut resp = match editor.status.lock().unwrap().clone() {
                Some(status) => Response::ok_status(status),
                None => Response::ok(),
            };
            resp.note = Some(if nvim.is_some() {
                "recto is in neovim".into()
            } else {
                "recto is in an editor".into()
            });
            resp
        }
        // Attaching public context only mutates TUI state. Queue it just like a
        // private note and let it become visible when the editor hands back.
        Request::AttachPr { .. } => {
            queue(tx, request.clone());
            Response::ok_note("recto is in an editor; PR context will open when you return")
        }
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
        // Annotations are a TUI rendering concern with no editor analogue;
        // just queue them for the main loop's return.
        Request::Annotate { .. } => {
            queue(tx, request.clone());
            Response::ok_note("recto is in an editor; annotations will apply when you return")
        }
        // Adding a comment is pure state, so queuing loses nothing: it lands
        // when the loop resumes. This is the `!recto note …` path out of
        // neovim, so it has to keep working while we're parked here.
        Request::AgentNote { .. } => {
            queue(tx, request.clone());
            Response::ok_note("recto is in an editor; the comment will land when you return")
        }
        // A drain must never be queued. `queue` discards the response, and a
        // drained comment is gone from recto — queuing one would delete the
        // user's notes and hand them to nobody. Refuse instead.
        Request::AgentNotes => {
            Response::err("recto is in an editor; leave it before draining agent notes")
        }
        Request::ReviewDraft => {
            Response::err("recto is in an editor; leave it before reading the review draft")
        }
        Request::ReviewDraftComment { .. } => {
            queue(tx, request.clone());
            Response::ok_note(
                "recto is in an editor; the shared draft update will land when you return",
            )
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

    /// Pin the annotate wire shape — companion agents hand-write this JSON, so
    /// field names and optionality are a compatibility contract.
    #[test]
    fn annotate_wire_format() {
        let json = r#"{"cmd":"annotate","sites":[
            {"path":"src/main.rs","start":3,"label":"Step 1: parse"},
            {"path":"src/link.rs","start":10,"end":14,"label":"Step 2: send"}
        ]}"#;
        let req: Request = serde_json::from_str(json).expect("parse");
        let Request::Annotate { sites } = req else {
            panic!("expected Annotate, got {req:?}");
        };
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].end, None);
        assert_eq!(sites[1].end, Some(14));
        assert_eq!(sites[1].label, "Step 2: send");
    }

    #[test]
    fn shared_review_draft_wire_format() {
        let create = r#"{"cmd":"review-comment","path":"src/main.rs","start":42,"body":"Could this return the error?"}"#;
        let req: Request = serde_json::from_str(create).expect("parse create");
        let Request::ReviewDraftComment {
            id,
            path,
            start,
            body,
            ..
        } = req
        else {
            panic!("expected shared review comment request");
        };
        assert_eq!(id, None);
        assert_eq!(path.as_deref(), Some("src/main.rs"));
        assert_eq!(start, Some(42));
        assert_eq!(body, "Could this return the error?");

        let revise =
            r#"{"cmd":"review-comment","id":7,"body":"Please return the error directly."}"#;
        let req: Request = serde_json::from_str(revise).expect("parse revision");
        let Request::ReviewDraftComment { id, path, body, .. } = req else {
            panic!("expected shared review comment revision");
        };
        assert_eq!(id, Some(7));
        assert_eq!(path, None);
        assert_eq!(body, "Please return the error directly.");
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"cmd":"review"}"#),
            Ok(Request::ReviewDraft)
        ));
    }

    #[test]
    fn shared_review_draft_response_preserves_editor_identity() {
        let response = Response::ok_review_draft(ReviewDraft {
            pull_request: PullRequestRef {
                repository: "phinze/recto".into(),
                number: 7,
                head_oid: "abc123".into(),
            },
            comments: vec![DraftReviewComment {
                id: 1,
                path: "src/main.rs".into(),
                start: 42,
                end: 42,
                body: "Shared words.".into(),
                last_editor: DraftEditor::Agent,
            }],
        });
        let json = serde_json::to_string(&response).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("round trip");
        let draft = back.review_draft.expect("review draft present");
        assert_eq!(draft.comments[0].id, 1);
        assert_eq!(draft.comments[0].last_editor, DraftEditor::Agent);
    }

    /// Pin the ping/status wire shape: companion agents parse this JSON, so the
    /// field names are a contract. Also guards backward compatibility — a plain
    /// `ok()` must not emit a `status` key, so older clients see the old shape.
    #[test]
    fn status_wire_format() {
        let resp = Response::ok_status(Status {
            version: "0.1.0".into(),
            pid: 4321,
            backend: "jj".into(),
            workspace_root: "/home/me/repo".into(),
            base: "@-".into(),
            scope: "range".into(),
            files: vec!["src/main.rs".into(), "src/link.rs".into()],
            surface: Surface::Recto,
            capabilities: Capabilities::recto(),
            focus: false,
            annotations: 0,
            pending_comments: 2,
            draft_comments: 0,
            pull_request: None,
        });
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("round trip");
        let status = back.status.expect("status present");
        assert_eq!(status.backend, "jj");
        assert_eq!(status.base, "@-");
        assert_eq!(status.files, vec!["src/main.rs", "src/link.rs"]);
        assert_eq!(status.pending_comments, 2);
        assert_eq!(status.surface, Surface::Recto);
        assert_eq!(
            status.capabilities.focus,
            Capability::live(TargetScope::CurrentDiff)
        );

        // A newer client may talk to a recto process left running across an
        // upgrade. Missing capability fields mean the old, diff-bound surface.
        let old_status = r#"{
            "version":"0.1.0",
            "pid":4321,
            "backend":"jj",
            "workspace_root":"/home/me/repo",
            "base":"@-",
            "scope":"range",
            "files":["src/main.rs"],
            "focus":false,
            "annotations":0
        }"#;
        let old: Status = serde_json::from_str(old_status).expect("old status parses");
        assert_eq!(old.surface, Surface::Recto);
        assert_eq!(old.capabilities, Capabilities::recto());
        // A recto too old to know about comments simply has none pending.
        assert_eq!(old.pending_comments, 0);
        assert_eq!(old.draft_comments, 0);

        // A bare ok() stays status-free on the wire for older clients.
        assert!(
            !serde_json::to_string(&Response::ok())
                .expect("serialize ok")
                .contains("status")
        );
    }

    /// While recto is parked in an editor, a `ping` is answered on the listener
    /// thread from the snapshot captured at editor entry — status on the wire,
    /// plus the "in an editor" note. Proves the fast-path doesn't go silent.
    #[test]
    fn ping_in_editor_serves_snapshot() {
        let editor = EditorLink::default();
        editor.enter(
            None,
            Status {
                version: "0.1.0".into(),
                pid: 7,
                backend: "git".into(),
                workspace_root: "/tmp/repo".into(),
                base: "HEAD".into(),
                scope: "range".into(),
                files: vec!["a.rs".into()],
                surface: Surface::Recto,
                capabilities: Capabilities::recto(),
                focus: false,
                annotations: 0,
                pending_comments: 0,
                draft_comments: 0,
                pull_request: None,
            },
        );
        let (tx, _rx) = mpsc::channel::<Incoming>();
        let resp = handle_while_in_editor(&Request::Ping, &tx, &editor);
        assert!(resp.ok);
        assert_eq!(resp.note.as_deref(), Some("recto is in an editor"));
        let status = resp.status.expect("status present while in editor");
        assert_eq!(status.backend, "git");
        assert_eq!(status.files, vec!["a.rs"]);
        assert_eq!(status.surface, Surface::Editor);
        assert_eq!(
            status.capabilities.focus,
            Capability::deferred(TargetScope::CurrentDiff)
        );

        // After leaving, the snapshot is dropped; a ping then carries just the note.
        editor.leave();
        let resp = handle_while_in_editor(&Request::Ping, &tx, &editor);
        assert!(resp.ok);
        assert!(resp.status.is_none());
    }

    #[test]
    fn ping_in_neovim_reports_workspace_focus() {
        let editor = EditorLink::default();
        editor.enter(
            Some(NvimHandle {
                prog: "nvim".into(),
                addr: PathBuf::from("/tmp/recto-test-nvim.sock"),
            }),
            Status {
                version: "0.1.0".into(),
                pid: 7,
                backend: "jj".into(),
                workspace_root: "/tmp/repo".into(),
                base: "@-".into(),
                scope: "range".into(),
                files: vec!["changed.rs".into()],
                surface: Surface::Recto,
                capabilities: Capabilities::recto(),
                focus: false,
                annotations: 0,
                pending_comments: 0,
                draft_comments: 0,
                pull_request: None,
            },
        );
        let (tx, _rx) = mpsc::channel::<Incoming>();
        let resp = handle_while_in_editor(&Request::Ping, &tx, &editor);
        assert!(resp.ok);
        assert_eq!(resp.note.as_deref(), Some("recto is in neovim"));
        let status = resp.status.expect("status present while in neovim");
        assert_eq!(status.surface, Surface::Neovim);
        assert_eq!(
            status.capabilities.focus,
            Capability::live(TargetScope::Workspace)
        );
        assert_eq!(
            status.capabilities.annotate,
            Capability::deferred(TargetScope::CurrentDiff)
        );
    }

    /// Pin the comment wire shape alongside `annotate`'s: reviewers reach this
    /// through the CLI, but `!recto note …` from an editor hand-writes it.
    #[test]
    fn comment_wire_format() {
        let json = r#"{"cmd":"comment","path":"src/main.rs","start":42,"body":"why 3?"}"#;
        let req: Request = serde_json::from_str(json).expect("parse");
        let Request::AgentNote {
            path,
            start,
            end,
            body,
        } = req
        else {
            panic!("expected Comment, got {req:?}");
        };
        assert_eq!(path, "src/main.rs");
        assert_eq!(start, 42);
        assert_eq!(end, None);
        assert_eq!(body, "why 3?");

        let drain: Request = serde_json::from_str(r#"{"cmd":"comments"}"#).expect("parse");
        assert!(matches!(drain, Request::AgentNotes));
    }

    /// The drained payload is what the agent actually reads, so its field names
    /// are a contract — including the per-row snippet shape.
    #[test]
    fn drained_comment_wire_format() {
        let resp = Response::ok_agent_notes(vec![AgentNote {
            n: 1,
            path: "src/main.rs".into(),
            start: 42,
            end: 42,
            body: "why 3?".into(),
            snippet: Some(vec![
                SnippetRow {
                    line: Some(41),
                    sign: ' ',
                    text: "let a = 1;".into(),
                    commented: false,
                },
                SnippetRow {
                    line: None,
                    sign: '-',
                    text: "let b = 2;".into(),
                    commented: true,
                },
                SnippetRow {
                    line: Some(42),
                    sign: '+',
                    text: "let b = 3;".into(),
                    commented: true,
                },
            ]),
        }]);
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("round trip");
        let comments = back.comments.expect("comments present");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].n, 1);
        assert_eq!(comments[0].body, "why 3?");
        let rows = comments[0].snippet.as_ref().expect("snippet present");
        assert_eq!(rows.len(), 3);
        // Removed rows carry the sign but no new-side number, and drop the key
        // entirely on the wire rather than emitting a null.
        assert_eq!(rows[1].sign, '-');
        assert_eq!(rows[1].line, None);
        assert!(!json.contains("\"line\":null"));

        // A bare ok() stays comments-free, so `ping` and friends are unchanged.
        assert!(
            !serde_json::to_string(&Response::ok())
                .expect("serialize ok")
                .contains("comments")
        );
    }

    /// A drain that arrives while recto is parked in an editor must be refused,
    /// never queued: `queue` throws the response away, so queuing a drain would
    /// clear the user's comments and deliver them nowhere.
    #[test]
    fn drain_in_editor_is_refused_not_queued() {
        let editor = EditorLink::default();
        editor.enter(
            None,
            Status {
                version: "0.1.0".into(),
                pid: 7,
                backend: "jj".into(),
                workspace_root: "/tmp/repo".into(),
                base: "@-".into(),
                scope: "range".into(),
                files: vec!["a.rs".into()],
                surface: Surface::Recto,
                capabilities: Capabilities::recto(),
                focus: false,
                annotations: 0,
                pending_comments: 3,
                draft_comments: 0,
                pull_request: None,
            },
        );
        let (tx, rx) = mpsc::channel::<Incoming>();

        let resp = handle_while_in_editor(&Request::AgentNotes, &tx, &editor);
        assert!(!resp.ok, "drain must fail while parked in an editor");
        assert!(resp.comments.is_none());
        assert!(
            rx.try_recv().is_err(),
            "a drain must not reach the main loop's queue"
        );

        // Authoring, by contrast, is queued and lands on return.
        let resp = handle_while_in_editor(
            &Request::AgentNote {
                path: "a.rs".into(),
                start: 1,
                end: None,
                body: "note".into(),
            },
            &tx,
            &editor,
        );
        assert!(resp.ok);
        let queued = rx.try_recv().expect("comment reaches the main loop");
        assert!(matches!(queued.request, Request::AgentNote { .. }));
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
