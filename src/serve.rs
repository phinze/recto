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

use std::collections::{HashMap, HashSet};
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

mod review;
pub(crate) use review::respond_offline as review_offline;

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
    /// Render only this root's section: what a page asks for when the reader
    /// moves one repo's view, so the rest of the page isn't rendered again.
    only: Option<usize>,
    /// Other groups whose open asks this page gathers, for a coordinator
    /// that organizes the person's attention across them.
    peers: Vec<Peer>,
}

/// Another group (a rig) whose briefing's asks a coordinator's page gathers.
/// The slug names it to whoever delivers answers and switches screens.
struct Peer {
    slug: String,
    label: String,
    basedir: String,
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
    let mut only = None;
    let mut peers = Vec::new();
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
            "only" => only = percent_decode(v).parse().ok(),
            "view" => {
                if let Some((i, spec)) = percent_decode(v).split_once(':')
                    && let Ok(i) = i.parse()
                {
                    views.push((i, spec.to_string()));
                }
            }
            // slug, basedir, label, tab-separated.
            "peer" => {
                let v = percent_decode(v);
                let mut f = v.splitn(3, '\t');
                if let (Some(slug), Some(basedir)) = (f.next(), f.next()) {
                    let label = f.next().filter(|l| !l.is_empty()).unwrap_or(slug);
                    peers.push(Peer {
                        slug: slug.to_string(),
                        label: label.to_string(),
                        basedir: basedir.to_string(),
                    });
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
        only,
        peers,
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
        PELE_PATH => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: private, max-age=31536000, immutable\r\nConnection: close\r\n\r\n",
                PELE.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(PELE.as_bytes());
        }
        path if path.starts_with("/asset/") => asset(&mut stream, &path["/asset/".len()..]),
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found\n"),
    }
}

/// A page never loads anything from off this server: the renderer turns
/// remote images into links, and this says the same to the browser.
const PAGE_POLICY: &str =
    "img-src 'self'; media-src 'none'; object-src 'none'; frame-src 'none'; font-src 'self'";

/// A stored image, by the name recto gave it. Content-addressed, so the
/// browser may keep it forever. An SVG opened directly runs no script and
/// fetches nothing; shown through `<img>` it couldn't anyway.
fn asset(stream: &mut UnixStream, name: &str) {
    let found = crate::assets::content_type(name).and_then(|ctype| {
        let dir = crate::assets::store_dir().ok()?;
        Some((ctype, std::fs::read(dir.join(name)).ok()?))
    });
    let Some((ctype, body)) = found else {
        respond(stream, "404 Not Found", "text/plain", b"not found\n");
        return;
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: private, max-age=31536000, immutable\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; sandbox\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn respond(stream: &mut UnixStream, status: &str, ctype: &str, body: &[u8]) {
    let policy = if ctype.starts_with("text/html") {
        format!("Content-Security-Policy: {PAGE_POLICY}\r\n")
    } else {
        String::new()
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{policy}Connection: close\r\n\r\n",
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
    for peer in &req.peers {
        watch_state(Path::new(&peer.basedir));
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
    // A group with no repositories (a rig that coordinates rather than
    // codes) still has a briefing and docs worth showing.
    if roots.is_empty() && req.group.is_none() && req.peers.is_empty() {
        return "<p class=empty>No workspaces named. Ask for <code>?root=/path/to/repo</code>, once per repository.</p>".into();
    }
    // One highlight budget per response, split between the repos and the
    // group tour, so a rig with many repos is no slower than one big diff.
    let share = HIGHLIGHT_BUDGET / (roots.len() + usize::from(req.group.is_some()));
    // One repo's section alone, for a page moving that repo's view. Its
    // budget matches the full page's, so the two render alike and share the
    // section cache.
    if let Some(i) = req.only {
        let Some(root) = roots.get(i) else {
            return String::new();
        };
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
        return workspace(i, root, start, view, share).0;
    }
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
    let mut docs: Vec<(String, String)> = Vec::new();
    let gathered = gather_asks(&req.peers);
    if let Some(group) = &req.group {
        group_docs(
            Path::new(group),
            &gathered,
            &rendered,
            &mut budget,
            &mut out,
            &mut docs,
        );
    } else if !gathered.is_empty() {
        brief_with(&gathered, "", &mut out);
    }
    if roots.is_empty() && docs.is_empty() && out.is_empty() {
        out.push_str("<p class=empty>Nothing here yet. This group has no repositories, and no briefing or docs have been written for it (<code>recto brief</code>, <code>recto doc NAME</code>).</p>");
    }
    let tabbed = roots.len() + docs.len() > 1;
    if tabbed {
        // Docs and repos read as tabs, one shown at a time. The page picks
        // which; the server only suggests a default: the group's tour, else
        // a repo with its own tour, else one with changes, else the first.
        let default = if docs.iter().any(|(name, _)| name == "tour") {
            "doc-tour".to_string()
        } else if rendered.is_empty() {
            format!("doc-{}", docs[0].0)
        } else {
            let i = rendered
                .iter()
                .position(|(_, s)| s.tour)
                .or_else(|| rendered.iter().position(|(_, s)| s.files > 0))
                .unwrap_or(0);
            format!("repo-{i}")
        };
        out.push_str("<div class=tabbed><nav class=tabs>");
        for (name, _) in &docs {
            let id = format!("doc-{name}");
            let _ = write!(
                out,
                "<button type=button class=doc-tab data-tab=\"{id}\"{}>{}</button>",
                if id == default { " data-default" } else { "" },
                esc(&doc_title(name)),
            );
        }
        for (i, (root, (_, sum))) in roots.iter().zip(&rendered).enumerate() {
            let _ = write!(
                out,
                "<button type=button data-tab=\"repo-{i}\"{}>{}{}",
                if format!("repo-{i}") == default {
                    " data-default"
                } else {
                    ""
                },
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
    for (name, html) in &docs {
        let _ = write!(
            out,
            "<section class=doc id=\"doc-{name}\" data-doc=\"{name}\">{html}</section>"
        );
    }
    for (html, _) in &rendered {
        out.push_str(html);
    }
    if tabbed {
        out.push_str("</div>");
    }
    let _ = write!(out, "<footer>rendered {}</footer>", chrono_now());
    out
}

/// A doc's name as its tab reads.
fn doc_title(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(first) => first
            .to_uppercase()
            .chain(c)
            .collect::<String>()
            .replace('-', " "),
        None => String::new(),
    }
}

/// The group's own documents, quoting from any of its repos: the briefing,
/// pinned above everything into `out`, and the rest (the tour first) as
/// `(name, html)` for tabs beside the repos. Each quote's path starts with
/// its repo's directory under the group, which is how they're written.
fn group_docs(
    group: &Path,
    gathered: &str,
    rendered: &[(String, Summary)],
    budget: &mut usize,
    out: &mut String,
    docs: &mut Vec<(String, String)>,
) {
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
    let quoted = crate::anchor::settle_group_quotes(
        &group,
        &crate::cli::group_repos(&group),
        store.quotes(),
    );
    let ctx = GroupQuotes {
        group: &group,
        rendered,
        quoted: &quoted,
    };
    match store.brief().filter(|t| !t.trim().is_empty()) {
        Some(brief) => {
            let mut html = String::new();
            render_group_doc(&ctx, "brief", "", brief, budget, &mut html);
            brief_with(gathered, &html, out);
        }
        None if !gathered.is_empty() => brief_with(gathered, "", out),
        None => {}
    }
    for doc in store.docs() {
        if doc.body.trim().is_empty() {
            continue;
        }
        let mut html = String::new();
        let class = if doc.name == "tour" { "tour" } else { "doc" };
        render_group_doc(&ctx, class, &doc.name, &doc.body, budget, &mut html);
        docs.push((doc.name, html));
    }
}

/// The briefing with the asks gathered from other groups leading it. `brief`
/// is the group's own, already wrapped, or empty when it hasn't written one.
fn brief_with(gathered: &str, brief: &str, out: &mut String) {
    const STRIP: &str = "<summary class=brief-strip>Briefing</summary>";
    if gathered.is_empty() {
        out.push_str(brief);
    } else if let Some(at) = brief.find(STRIP) {
        let at = at + STRIP.len();
        out.push_str(&brief[..at]);
        out.push_str(gathered);
        out.push_str(&brief[at..]);
    } else {
        let _ = write!(
            out,
            "<details class=group-brief open>{STRIP}{gathered}</details>"
        );
    }
}

/// Every ask in the peers' briefings, under a link to the rig that asked.
/// Whether each is answered is the page's call, from the comments it's
/// given; an answer goes to the rig in the card's `data-rig`.
fn gather_asks(peers: &[Peer]) -> String {
    let mut out = String::new();
    for peer in peers {
        let Ok(dir) = Path::new(&peer.basedir).canonicalize() else {
            continue;
        };
        let Ok(store) = state::Store::load(&dir, None) else {
            let _ = write!(
                out,
                "<div class=peer data-rig=\"{0}\"><h3><a href=\"rig:{0}\">{1}</a></h3><p class=error>could not read its briefing</p></div>",
                esc(&peer.slug),
                esc(&peer.label)
            );
            continue;
        };
        let asks = store.brief().map(brief_asks).unwrap_or_default();
        if asks.is_empty() {
            continue;
        }
        let _ = write!(
            out,
            "<div class=peer data-rig=\"{0}\"><h3><a href=\"rig:{0}\">{1}</a></h3>",
            esc(&peer.slug),
            esc(&peer.label)
        );
        for (kind, id, text) in asks {
            out.push_str(&ask_html(kind, &id, &text, Some(&peer.slug)));
        }
        out.push_str("</div>");
    }
    if out.is_empty() {
        return out;
    }
    format!("<section class=gathered><h2>Across rigs</h2>{out}</section>")
}

/// What needs the person, as a briefing writes it: an `ask` is a question
/// they answer, a `do` is an action of theirs they mark done. Both keep the
/// briefing open until they're settled, and both are answered the same way.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    Ask,
    Do,
}

/// Read a fence's info string as an `ask` or `do` block: its kind and the id
/// it names, if any.
fn need_fence(info: &str) -> Option<(Need, Option<String>)> {
    let info = info.trim();
    let (kind, rest) = if let Some(rest) = info.strip_prefix("ask") {
        (Need::Ask, rest)
    } else if let Some(rest) = info.strip_prefix("do") {
        (Need::Do, rest)
    } else {
        return None;
    };
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    Some((kind, rest.split_whitespace().next().map(str::to_string)))
}

/// The id a block names, or its place among the briefing's blocks.
fn need_id(kind: Need, id: Option<String>, n: usize) -> String {
    id.unwrap_or_else(|| match kind {
        Need::Ask => format!("ask{n}"),
        Need::Do => format!("do{n}"),
    })
}

/// The asks and to-dos in a briefing, as `(kind, id, text)`, numbered the way
/// `render_doc` numbers a block that names no id.
fn brief_asks(source: &str) -> Vec<(Need, String, String)> {
    let mut out = Vec::new();
    let mut asking: Option<(Need, String, String)> = None;
    let mut n = 0;
    for event in Parser::new_ext(source, Options::all() - Options::ENABLE_SMART_PUNCTUATION) {
        if let Some((_, _, text)) = asking.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => out.extend(asking.take()),
                Event::Text(t) => text.push_str(&t),
                _ => {}
            }
            continue;
        }
        if let Event::Start(Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(info))) = event
            && let Some((kind, id)) = need_fence(&info)
        {
            n += 1;
            asking = Some((kind, need_id(kind, id, n), String::new()));
        }
    }
    out
}

/// What a group document's quotes resolve against: the group, its repos'
/// summaries, and where each stamped quote's lines are now.
struct GroupQuotes<'a> {
    group: &'a Path,
    rendered: &'a [(String, Summary)],
    quoted: &'a HashMap<String, crate::anchor::Quoted>,
}

fn render_group_doc(
    ctx: &GroupQuotes,
    class: &str,
    name: &str,
    tour: &str,
    budget: &mut usize,
    out: &mut String,
) {
    let GroupQuotes {
        group,
        rendered,
        quoted,
    } = *ctx;
    let repos: Vec<(usize, String)> = rendered
        .iter()
        .enumerate()
        .filter_map(|(i, (_, sum))| {
            let rel = sum.root.as_ref()?.strip_prefix(group).ok()?;
            Some((i, rel.to_string_lossy().into_owned()))
        })
        .collect();
    let label = if class == "brief" {
        "Briefing".to_string()
    } else if class == "doc" {
        doc_title(name)
    } else if repos.len() > 1 {
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
    // The briefing folds to a strip once nothing in it needs the person; the
    // page decides that, since only it knows which asks are answered.
    if class == "brief" {
        out.push_str(
            "<details class=group-brief open><summary class=brief-strip>Briefing</summary>",
        );
    } else {
        let _ = write!(out, "<div class=group-{class}>");
    }
    let ids = match class {
        "brief" => "bg".to_string(),
        "tour" => "tg".to_string(),
        _ => format!("d-{name}"),
    };
    render_doc(
        class,
        &ids,
        &label,
        tour,
        budget,
        out,
        &mut |spec, budget, html| {
            // A stamped quote lifts the lines it was written about, wherever
            // they sit now; its spec moves with them.
            let moved = match quoted.get(spec.trim()) {
                Some(crate::anchor::Quoted::Outdated) => {
                    outdated_quote(spec, html);
                    return;
                }
                Some(crate::anchor::Quoted::At(now)) => now.as_str(),
                Some(crate::anchor::Quoted::Elsewhere) | None => spec,
            };
            let hit = repos.iter().find_map(|(i, rel)| {
                let rest = moved.strip_prefix(rel.as_str())?.strip_prefix('/')?;
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
    out.push_str(if class == "brief" {
        "</details>"
    } else {
        "</div>"
    });
}

fn repo_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// What a tab needs to know about a workspace without reading its page.
#[derive(Clone, Default)]
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

/// Rendered sections, keyed by everything a section depends on: the repo's
/// latest jj operation (which a working-copy edit advances, since asking
/// for it snapshots), its saved state, and the request's own view, start and
/// budget. Moving back and forth through a stack, or refreshing a page where
/// only one repo changed, then costs one jj call per unchanged section.
static SECTIONS: std::sync::Mutex<Option<HashMap<String, (String, Summary)>>> =
    std::sync::Mutex::new(None);
const SECTIONS_KEPT: usize = 64;

/// The cache key for a section, or `None` where freshness can't be told
/// cheaply (a git repository) and the section is always rendered.
fn section_key(
    index: usize,
    root: &str,
    start: Option<&str>,
    view: Option<&str>,
    budget: usize,
) -> Option<String> {
    let root = canonical_repo(Path::new(root))?;
    if !root.join(".jj").is_dir() {
        return None;
    }
    let out = std::process::Command::new("jj")
        .args(["op", "log", "--limit", "1", "--no-graph", "-T", "self.id()"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let op = String::from_utf8(out.stdout).ok()?;
    let state = state::state_path(&root)
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| format!("{:?}:{}", m.modified().ok(), m.len()))
        .unwrap_or_default();
    Some(format!(
        "{index}\x1f{}\x1f{}\x1f{}\x1f{budget}\x1f{}\x1f{state}",
        root.display(),
        start.unwrap_or(""),
        view.map_or("\x1e", |v| v),
        op.trim()
    ))
}

fn workspace(
    index: usize,
    root: &str,
    start: Option<&str>,
    view: Option<&str>,
    budget: usize,
) -> (String, Summary) {
    let key = section_key(index, root, start, view, budget);
    if let Some(key) = &key
        && let Some(hit) = SECTIONS
            .lock()
            .ok()
            .and_then(|c| c.as_ref().and_then(|c| c.get(key).cloned()))
    {
        return hit;
    }
    let rendered = render_section(index, root, start, view, budget);
    if let Some(key) = key
        && !rendered.1.error
        && let Ok(mut cache) = SECTIONS.lock()
    {
        let cache = cache.get_or_insert_with(HashMap::new);
        if cache.len() >= SECTIONS_KEPT {
            cache.clear();
        }
        cache.insert(key, rendered.clone());
    }
    rendered
}

fn render_section(
    index: usize,
    root: &str,
    start: Option<&str>,
    view: Option<&str>,
    budget: usize,
) -> (String, Summary) {
    let mut out = String::new();
    let _ = write!(
        out,
        "<section class=repo id=\"repo-{index}\" data-repo=\"{}\">",
        esc(&repo_name(Path::new(root)))
    );
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
    // A stack of PRs is ranged from its bottom PR's base, so every layer is
    // on the page; the front PR's own base would cut off the ones below it.
    let branch_base = view_base(
        backend.as_ref(),
        store.as_ref().and_then(|s| s.stack_base_pr()),
    )?;
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
    // The view names a change or a layer by change, so it follows its work
    // through a rewrite. The reader's own pick wins; without one, the page
    // shows the view the agent last moved it to.
    let agent_lens = store.as_ref().and_then(|s| s.lens().map(str::to_string));
    let lens_view = crate::stack::view(&commits, view.or(agent_lens.as_deref()));
    let shown = match &lens_view {
        crate::stack::View::Change(c) => parse_diff(
            &backend
                .unified_diff(&Scope::Rev(c.id.clone()), false)
                .context("diff failed")?,
        ),
        crate::stack::View::Layer(l) => parse_diff(
            &backend
                .diff_span(&l.bottom().id, &l.head().id)
                .context("diff failed")?,
        ),
        crate::stack::View::Stack => Vec::new(),
    };
    // Every attached PR, and the one the layer in view carries, with its
    // review: drafts resolved against just that layer, the way the PR reads.
    let prs: Vec<link::PullRequest> = store
        .as_ref()
        .map(|s| s.pull_requests().into_iter().cloned().collect())
        .unwrap_or_default();
    let layer_review = match &lens_view {
        crate::stack::View::Layer(l) => layer_pr(&prs, l).map(|pr| {
            let review = store.as_ref().and_then(|s| {
                s.review(&link::PullRequestRef {
                    repository: pr.repository.clone(),
                    number: pr.number,
                    head_oid: pr.head_oid.clone(),
                })
            });
            // Read at the layer's head without narrowing to its own commits:
            // a draft on unchanged lines inside a hunk is stamped to an older
            // change, and still belongs on this layer's diff. Whether its
            // lines are on screen is the diff's to say.
            let review = review.map(|mut r| {
                let lens = crate::anchor::Lens {
                    head: l.head().id.clone(),
                    members: None,
                };
                crate::anchor::settle(backend.as_ref(), &lens, &mut r.comments);
                r
            });
            (pr.clone(), review)
        }),
        _ => None,
    };
    let layer_drafts: Vec<link::DraftReviewComment> = layer_review
        .as_ref()
        .and_then(|(_, r)| r.as_ref().map(|r| r.comments.clone()))
        .unwrap_or_default();
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
    // A review in progress on the attached PR: its drafts show where they'll
    // land, so the person can steer them with comments like any other text.
    let lens = crate::anchor::Lens::range(backend.as_ref());
    let quoted: HashMap<String, crate::anchor::Quoted> = store
        .as_ref()
        .map(|s| {
            let mut quotes = s.quotes().to_vec();
            crate::anchor::settle(backend.as_ref(), &lens, &mut quotes);
            quotes
                .iter()
                .map(|q| (q.spec.clone(), q.quoted()))
                .collect()
        })
        .unwrap_or_default();
    let review = pull_request
        .as_ref()
        .and_then(|pr| {
            store.as_ref()?.review(&link::PullRequestRef {
                repository: pr.repository.clone(),
                number: pr.number,
                head_oid: pr.head_oid.clone(),
            })
        })
        .map(|mut r| {
            crate::anchor::settle(backend.as_ref(), &lens, &mut r.comments);
            r
        });
    // One review per view, so feedback on it can't land on another: a
    // layer carrying its own PR shows that PR's review in its place.
    let front_pr = pull_request.as_ref().map(|p| p.number);
    let layer_pr_number = layer_review.as_ref().map(|(p, _)| p.number);
    if let Some(r) = review
        .as_ref()
        .filter(|r| r.verdict.is_some() || r.body.is_some() || !r.comments.is_empty())
        .filter(|_| layer_pr_number.is_none_or(|n| Some(n) == front_pr))
    {
        review_card(r, &files, front_pr, out);
    }
    let drafts: &[link::DraftReviewComment] = review.as_ref().map_or(&[], |r| &r.comments);
    // Where the agent last moved the view, for the page to follow when it
    // changes, the way it follows focus.
    if store.is_some() {
        let _ = write!(
            out,
            "<i class=lens-marker hidden data-lens=\"{index}:{}\"></i>",
            esc(agent_lens.as_deref().unwrap_or(""))
        );
    }
    if commits.len() > 1 {
        let head = backend.commit_id(backend.head_rev());
        render_stack(
            index,
            &commits,
            &lens_view,
            &StackCtx {
                prs: &prs,
                head: head.as_deref(),
                from_start: anchor.label.is_some(),
                totals: (added, removed),
            },
            out,
        );
    }
    // The saved focus, for the page to highlight and, when it changes, scroll
    // to. A span in a file that left the diff has nowhere to point.
    let focus = store.as_ref().and_then(|s| s.focus()).cloned().map(|f| {
        let mut one = [f];
        crate::anchor::settle(backend.as_ref(), &lens, &mut one);
        let [f] = one;
        f
    });
    if let Some(focus) = focus.filter(|f| f.pin.shown())
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
            &mut |spec, budget, html| match quoted.get(spec.trim()) {
                Some(crate::anchor::Quoted::Outdated) => outdated_quote(spec, html),
                Some(crate::anchor::Quoted::At(now)) => {
                    quote_html(index, now, spec, &files, budget, html)
                }
                Some(crate::anchor::Quoted::Elsewhere) | None => {
                    quote_html(index, spec, spec, &files, budget, html)
                }
            },
        );
    }
    if let Some((pr, review)) = &layer_review {
        let _ = write!(
            out,
            "<div class=layer-pr><a href=\"{}\">#{}</a> {} <span class=dim>{} ← {}</span></div>",
            esc(&pr.url),
            pr.number,
            esc(&pr.title),
            esc(&pr.base_ref),
            esc(&pr.head_ref),
        );
        if let Some(r) = review
            .as_ref()
            .filter(|r| r.verdict.is_some() || r.body.is_some() || !r.comments.is_empty())
            .filter(|_| Some(pr.number) != front_pr)
        {
            review_card(r, &shown, Some(pr.number), out);
        }
    }
    if !matches!(lens_view, crate::stack::View::Stack) {
        if shown.is_empty() {
            out.push_str(match lens_view {
                crate::stack::View::Layer(_) => "<p class=empty>This layer changes no files.</p>",
                _ => "<p class=empty>This commit changes no files.</p>",
            });
        } else {
            render_files(
                index,
                &shown,
                &layer_drafts,
                layer_pr_number,
                &mut budget,
                out,
            );
        }
    } else if !files.is_empty() {
        render_files(index, &files, drafts, front_pr, &mut budget, out);
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
/// The stack: a sticky bar naming the view with a step to the change on
/// either side, then the layers, each a PR or one meant to be, with their
/// commits. A stack with one layer lists its commits as before. Landed
/// layers fold, since their work is done.
/// What the stack is drawn around besides its commits: the PRs attached to
/// its layers, the working copy's commit, whether the range reaches back to
/// the rig's start, and the range's own totals.
struct StackCtx<'a> {
    prs: &'a [link::PullRequest],
    head: Option<&'a str>,
    from_start: bool,
    totals: (u32, u32),
}

/// The PR a layer carries: the attached PR whose head is one of the layer's
/// commits, its head commit first.
fn layer_pr<'a>(
    prs: &'a [link::PullRequest],
    layer: &crate::stack::Layer,
) -> Option<&'a link::PullRequest> {
    let is = |c: &Commit, pr: &link::PullRequest| c.id.eq_ignore_ascii_case(&pr.head_oid);
    prs.iter().find(|pr| is(layer.head(), pr)).or_else(|| {
        prs.iter()
            .find(|pr| layer.commits.iter().any(|c| is(c, pr)))
    })
}

fn render_stack(
    index: usize,
    commits: &[Commit],
    view: &crate::stack::View,
    ctx: &StackCtx,
    out: &mut String,
) {
    use crate::stack::View;
    let StackCtx {
        prs,
        head,
        from_start,
        totals: (added, removed),
    } = *ctx;
    let layers = crate::stack::layers(commits);
    let layer_of = |c: &Commit| layers.iter().find(|l| l.contains(c));
    // A layer carrying a PR goes by the PR; its bookmark is just plumbing.
    let label = |l: &crate::stack::Layer| match layer_pr(prs, l) {
        Some(pr) => format!("#{} {}", pr.number, pr.title),
        None => l.label(),
    };

    // The bar: where the view is, and a step older or newer through the
    // changes in reading order. Stepping off the newest comes back to the
    // whole stack, and off the whole stack starts at the oldest.
    let order: Vec<&Commit> = crate::stack::reading_order(commits).collect();
    let at = match view {
        View::Change(c) => order.iter().position(|o| o.id == c.id),
        _ => None,
    };
    let older = match (view, at) {
        (View::Change(_), Some(0)) | (View::Stack, _) => None,
        (View::Change(_), Some(i)) => Some(order[i - 1].target().to_string()),
        (View::Layer(l), _) => order
            .iter()
            .position(|o| o.id == l.bottom().id)
            .and_then(|i| i.checked_sub(1))
            .map(|i| order[i].target().to_string()),
        _ => None,
    };
    let newer = match (view, at) {
        (View::Stack, _) => order.first().map(|o| o.target().to_string()),
        (View::Change(_), Some(i)) => Some(
            order
                .get(i + 1)
                .map(|o| o.target().to_string())
                .unwrap_or_default(),
        ),
        (View::Layer(l), _) => order
            .iter()
            .position(|o| o.id == l.head().id)
            .and_then(|i| order.get(i + 1))
            .map(|o| o.target().to_string()),
        _ => None,
    };
    let crumb = match view {
        View::Stack => format!(
            "<b>whole stack</b><span class=dim> · {} commit{}{}</span>",
            commits.len(),
            if commits.len() == 1 { "" } else { "s" },
            if layers.len() > 1 {
                format!(" · {} layers", layers.len())
            } else {
                String::new()
            }
        ),
        View::Layer(l) => format!(
            "<button type=button class=crumb data-view=\"{index}:\">stack</button> › <b>{}</b><span class=dim> · {} commit{}</span>",
            esc(&label(l)),
            l.commits.len(),
            if l.commits.len() == 1 { "" } else { "s" }
        ),
        View::Change(c) => {
            let layer = layer_of(c).filter(|_| layers.len() > 1);
            format!(
                "<button type=button class=crumb data-view=\"{index}:\">stack</button> › {}<code>{}</code> <span class=lens-sum>{}</span>",
                layer
                    .map(|l| format!(
                        "<button type=button class=crumb data-view=\"{index}:{}\">{}</button> › ",
                        esc(&l.view()),
                        esc(&label(l))
                    ))
                    .unwrap_or_default(),
                esc(c.short_change()),
                esc(if c.summary.is_empty() {
                    "(no description)"
                } else {
                    &c.summary
                })
            )
        }
    };
    let step = |label: &str, glyph: &str, target: Option<String>| match target {
        Some(t) => format!(
            "<button type=button class=step data-view=\"{index}:{}\" aria-label=\"{label}\" title=\"{label}\">{glyph}</button>",
            esc(&t)
        ),
        None => format!(
            "<button type=button class=step disabled aria-label=\"{label}\">{glyph}</button>"
        ),
    };
    let _ = write!(
        out,
        "<div class=lensbar>{}<span class=crumbs>{crumb}</span>{}</div>",
        step("older change", "‹", older),
        step("newer change", "›", newer),
    );

    let landed = commits.iter().filter(|c| c.landed).count();
    let narrowed = !matches!(view, View::Stack);
    let _ = write!(
        out,
        "<details class=revs id=\"revs-{index}\"{}><summary>{} commits{}{}</summary><ol>",
        if narrowed { " open" } else { "" },
        commits.len(),
        if layers.len() > 1 {
            format!(" in {} layers", layers.len())
        } else {
            String::new()
        },
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
        "<li><button type=button data-view=\"{index}:\"{}><span class=rev-id></span><span class=rev-sum>whole stack {}</span>{}</button></li>",
        if narrowed { "" } else { " class=active" },
        if from_start {
            "since the rig started"
        } else {
            "in this range"
        },
        rev_stat(added, removed),
    );
    let row = |c: &Commit, out: &mut String| {
        let active = matches!(view, View::Change(v) if v.id == c.id);
        let _ = write!(
            out,
            "<li><button type=button data-view=\"{index}:{}\"{}{}><span class=rev-id>{}</span><span class=rev-sum>{}</span>{}<span class=rev-when>{}</span>{}</button></li>",
            esc(c.target()),
            if active { " class=active" } else { "" },
            if c.mine { "" } else { " data-foreign" },
            esc(c.short_change()),
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
    };
    if layers.len() <= 1 {
        for c in commits {
            row(c, out);
        }
    } else {
        for layer in &layers {
            let active = matches!(view, View::Layer(v) if v.head().id == layer.head().id);
            let (a, r) = layer
                .commits
                .iter()
                .fold((0, 0), |(a, r), c| (a + c.added, r + c.removed));
            let _ = write!(
                out,
                "<li class=layer><button type=button class=\"layer-head{}\" data-view=\"{index}:{}\"><span class=rev-id>{}</span><span class=rev-sum>{} commit{}</span>{}{}</button>",
                if active { " active" } else { "" },
                esc(&layer.view()),
                esc(&label(layer)),
                layer.commits.len(),
                if layer.commits.len() == 1 { "" } else { "s" },
                if layer.landed() {
                    "<span class=landed>landed</span>"
                } else {
                    ""
                },
                rev_stat(a, r),
            );
            // A landed layer folds unless the view is inside it.
            let inside = match view {
                View::Change(c) => layer.contains(c),
                View::Layer(v) => v.head().id == layer.head().id,
                View::Stack => false,
            };
            if layer.landed() && !inside {
                let _ = write!(
                    out,
                    "<details class=layer-fold><summary>{} landed commit{}</summary><ol>",
                    layer.commits.len(),
                    if layer.commits.len() == 1 { "" } else { "s" }
                );
            } else {
                out.push_str("<ol>");
            }
            for c in layer.commits {
                row(c, out);
            }
            out.push_str(if layer.landed() && !inside {
                "</ol></details></li>"
            } else {
                "</ol></li>"
            });
        }
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
        .or_else(|| backend.default_base())
        .ok_or_else(|| anyhow!("no diff base available"))
}

/// Where a rig repo's work started, from the environment `rig env` sets up:
/// `RIG_STARTS` pairs each repo's directory under `RIG_BASEDIR` with its
/// starting commit. `rig serve` hands the web view the same commits.
fn rig_start(root: &Path) -> Option<String> {
    let basedir = Path::new(&std::env::var_os("RIG_BASEDIR")?)
        .canonicalize()
        .ok()?;
    let name = root.strip_prefix(&basedir).ok()?.to_str()?.to_string();
    let starts = std::env::var("RIG_STARTS").ok()?;
    starts.split_whitespace().find_map(|pair| {
        let (dir, commit) = pair.split_once('=')?;
        (dir == name).then(|| commit.to_string())
    })
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
        // The same range the web view shows, anchored at the rig's start
        // when rig says where that is.
        let branch = view_base(backend.as_ref(), store.stack_base_pr())?;
        let mut files = parse_diff(&backend.unified_diff(&Scope::Range(branch.clone()), false)?);
        let start_commit = rig_start(backend.root());
        let anchored = anchor(
            backend.as_ref(),
            &branch,
            start_commit.as_deref(),
            files.is_empty(),
        );
        if anchored.base != branch {
            files = parse_diff(&backend.unified_diff(&Scope::Range(anchored.base), false)?);
        }
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
            pin: crate::anchor::Pin::stamped(crate::anchor::Resolver::new(backend.as_ref()).stamp(
                &crate::anchor::Lens::range(backend.as_ref()),
                path,
                start,
                end,
            )),
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
/// Move the view with no viewer running: the window follows the saved lens.
/// A spec that names nothing in the stack is refused, so a typo doesn't
/// quietly leave the window on the whole stack.
pub(crate) fn save_lens_offline(spec: &str) -> i32 {
    let result = (|| -> Result<std::result::Result<(), String>> {
        let backend = crate::backend::detect_backend()?;
        let mut store = state::Store::load(backend.root(), None)?;
        let spec = spec.trim();
        if !spec.is_empty() {
            let base = view_base(backend.as_ref(), store.stack_base_pr())?;
            let anchored = anchor(
                backend.as_ref(),
                &base,
                rig_start(backend.root()).as_deref(),
                false,
            );
            let commits = backend
                .base_commit(&anchored.base)
                .and_then(|from| backend.commits(&from, backend.head_rev()).ok())
                .unwrap_or_default();
            if matches!(
                crate::stack::view(&commits, Some(spec)),
                crate::stack::View::Stack
            ) {
                return Ok(Err(format!(
                    "{spec} names no change or layer in this stack (a change id or unique prefix, or layer:CHANGE)"
                )));
            }
        }
        store.set_lens(Some(spec));
        store.save()?;
        Ok(Ok(()))
    })();
    match result {
        Ok(Ok(())) => 0,
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

#[derive(Clone)]
struct DiffLine {
    kind: Kind,
    old: Option<u32>,
    new: Option<u32>,
    text: String,
}

#[derive(Clone)]
struct Hunk {
    header: String,
    lines: Vec<DiffLine>,
}

#[derive(Clone)]
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

fn render_files(
    index: usize,
    files: &[FileDiff],
    drafts: &[link::DraftReviewComment],
    pr: Option<u64>,
    budget: &mut usize,
    out: &mut String,
) {
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
            "<details class=file id=\"f{index}-{key}\" data-path=\"{}\"{open}><summary><span class=\"st st-{}\">{}</span><span class=path>{}</span><span class=stat><span class=add>+{}</span> <span class=del>−{}</span>{}</span></summary>",
            esc(&file.path),
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
                    // A review draft sits under the last line it's about,
                    // where GitHub will show it.
                    if let Some(n) = line.new {
                        for d in drafts
                            .iter()
                            .filter(|d| d.pin.shown() && d.path == file.path && d.end == n)
                        {
                            out.push_str("<tr class=draft><td colspan=3>");
                            draft_card(d, pr, out);
                            out.push_str("</td></tr>");
                        }
                    }
                }
            }
            out.push_str("</table>");
        }
        out.push_str("</details>");
    }
    out.push_str("</div>");
}

/// One inline review draft, as the reader will see it posted.
fn draft_card(d: &link::DraftReviewComment, pr: Option<u64>, out: &mut String) {
    let span = if d.start == d.end {
        d.start.to_string()
    } else {
        format!("{}-{}", d.start, d.end)
    };
    let who = match d.last_editor {
        link::DraftEditor::Agent => "agent",
        link::DraftEditor::User => "you",
    };
    let _ = write!(
        out,
        "<div class=draft-card{} data-draft=\"{}\" data-path=\"{}\" data-start={} data-end={}><div class=draft-head>Draft #{} <span class=dim>{}:{span} · last edit {who}</span>{}</div><div class=draft-body>{}</div></div>",
        pr.map(|n| format!(" data-pr={n}")).unwrap_or_default(),
        d.id,
        esc(&d.path),
        d.start,
        d.end,
        d.id,
        esc(&d.path),
        if d.pin.outdated {
            " <span class=outdated title=\"the lines it was written on changed since\">outdated</span>"
        } else {
            ""
        },
        small_markdown(&d.body)
    );
}

/// The review taking shape, at the top of its repo: verdict, summary, and
/// any draft whose lines have left the diff, so nothing silently vanishes.
fn review_card(
    review: &state::RestoredReview,
    files: &[FileDiff],
    pr: Option<u64>,
    out: &mut String,
) {
    let verdict = review.verdict.map_or("no verdict yet", |v| v.label());
    let n = review.comments.len();
    let _ = write!(
        out,
        "<article class=review{}><div class=review-head>Review draft{} · <b>{verdict}</b> · {n} inline comment{}</div>",
        pr.map(|n| format!(" data-pr={n}")).unwrap_or_default(),
        pr.map(|n| format!(" for #{n}")).unwrap_or_default(),
        if n == 1 { "" } else { "s" }
    );
    match review.body.as_ref().filter(|b| !b.body.trim().is_empty()) {
        Some(b) => {
            let _ = write!(
                out,
                "<div class=review-body>{}</div>",
                small_markdown(&b.body)
            );
        }
        None => out.push_str("<p class=dim>No summary yet.</p>"),
    }
    let lost: Vec<&link::DraftReviewComment> = review
        .comments
        .iter()
        .filter(|d| {
            !d.pin.shown()
                || !files
                    .iter()
                    .any(|f| f.path == d.path && review::shows_span(f, d.start, d.end))
        })
        .collect();
    if !lost.is_empty() {
        out.push_str("<div class=draft-lost><p class=dim>Not on this diff any more:</p>");
        for d in lost {
            draft_card(d, pr, out);
        }
        out.push_str("</div>");
    }
    out.push_str("</article>");
}

/// The page-relative URL for an image recto stored when its document was
/// saved. Relative, so it resolves the same behind rig's proxy as standalone.
fn stored_image(dest: &str) -> Option<String> {
    let name = dest.strip_prefix(crate::assets::SCHEME)?;
    crate::assets::content_type(name).map(|_| format!("asset/{name}"))
}

/// A short markdown body (a draft comment, a review summary) as HTML, with
/// the same rules as a tour: raw HTML shows as text, links only go somewhere
/// safe, and images never load.
fn small_markdown(body: &str) -> String {
    let events =
        Parser::new_ext(body, Options::all() - Options::ENABLE_SMART_PUNCTUATION).map(|event| {
            match event {
                Event::Html(t) | Event::InlineHtml(t) => Event::Text(t),
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => Event::Start(Tag::Link {
                    link_type,
                    dest_url: safe_url(dest_url),
                    title,
                    id,
                }),
                Event::Start(Tag::Image {
                    dest_url,
                    title,
                    id,
                    ..
                }) => Event::Start(Tag::Link {
                    link_type: pulldown_cmark::LinkType::Inline,
                    dest_url: safe_url(dest_url),
                    title,
                    id,
                }),
                Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
                other => other,
            }
        });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, autolink(events).into_iter());
    html
}

/// Turn bare http(s) URLs in prose into links, the way GitHub does: Markdown
/// itself only links `[text](url)` and `<url>`, so a URL pasted into a
/// briefing or a draft would otherwise sit there as text. Code and existing
/// links are left alone.
fn autolink<'a>(events: impl Iterator<Item = Event<'a>>) -> Vec<Event<'a>> {
    let mut out = Vec::new();
    let (mut in_code, mut in_link) = (0usize, 0usize);
    for event in pulldown_cmark::TextMergeStream::new(events) {
        match &event {
            Event::Start(Tag::CodeBlock(_)) => in_code += 1,
            Event::End(TagEnd::CodeBlock) => in_code = in_code.saturating_sub(1),
            Event::Start(Tag::Link { .. }) => in_link += 1,
            Event::End(TagEnd::Link) => in_link = in_link.saturating_sub(1),
            Event::Text(text) if in_code == 0 && in_link == 0 => {
                let mut rest: &str = text;
                while let Some((before, url, after)) = next_url(rest) {
                    if !before.is_empty() {
                        out.push(Event::Text(before.to_string().into()));
                    }
                    out.push(Event::Start(Tag::Link {
                        link_type: pulldown_cmark::LinkType::Autolink,
                        dest_url: url.to_string().into(),
                        title: "".into(),
                        id: "".into(),
                    }));
                    out.push(Event::Text(url.to_string().into()));
                    out.push(Event::End(TagEnd::Link));
                    rest = after;
                }
                if !rest.is_empty() {
                    out.push(Event::Text(rest.to_string().into()));
                }
                continue;
            }
            _ => {}
        }
        out.push(event);
    }
    out
}

/// The first bare http(s) URL in `text`, split as (before, url, after).
/// Trailing punctuation that ends a sentence stays out of the URL, and so
/// does a closing paren the URL didn't open.
fn next_url(text: &str) -> Option<(&str, &str, &str)> {
    let start = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| text.find(scheme))
        .min()?;
    let tail = &text[start..];
    let mut end = tail
        .find(|c: char| c.is_whitespace() || "<>\"`".contains(c))
        .unwrap_or(tail.len());
    loop {
        let url = &tail[..end];
        let last = url.chars().last()?;
        let unbalanced = last == ')' && url.matches('(').count() < url.matches(')').count();
        if ".,;:!?'*_".contains(last) || unbalanced {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    let url = &tail[..end];
    // A scheme with nothing after it is just the word.
    if url.len() <= "https://".len() {
        return None;
    }
    Some((&text[..start], url, &tail[end..]))
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
    render_doc("tour", ids, label, source, budget, out, quote);
}

/// An agent-written Markdown document: a tour, or the briefing. Headings make
/// an outline, fenced `recto PATH:SPAN` blocks quote the diff, and fenced
/// `ask ID` blocks are questions the person answers in place.
fn render_doc(
    class: &str,
    ids: &str,
    label: &str,
    source: &str,
    budget: &mut usize,
    out: &mut String,
    quote: &mut QuoteFn,
) {
    let mut asking: Option<(Need, String, String)> = None;
    let mut asks = 0;
    let parser = Parser::new_ext(source, Options::all() - Options::ENABLE_SMART_PUNCTUATION);
    let mut events: Vec<Event> = Vec::new();
    let mut outline: Vec<(String, String)> = Vec::new();
    let mut heading: Option<(String, String)> = None;
    let mut quoting: Option<String> = None;
    // Whether each open image is a stored one, so its end closes the right
    // element; alt text can itself hold an image.
    let mut images: Vec<bool> = Vec::new();
    let mut diagram: Option<String> = None;
    for event in parser {
        if let Some((_, _, text)) = asking.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    if let Some((kind, id, text)) = asking.take() {
                        events.push(Event::Html(ask_html(kind, &id, &text, None).into()));
                    }
                }
                Event::Text(t) => text.push_str(&t),
                _ => {}
            }
            continue;
        }
        if quoting.is_some() {
            if let Event::End(TagEnd::CodeBlock) = event {
                let spec = quoting.take().unwrap_or_default();
                let mut html = String::new();
                quote(spec.trim(), budget, &mut html);
                events.push(Event::Html(html.into()));
            }
            continue;
        }
        if let Some(text) = diagram.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    let text = diagram.take().unwrap_or_default();
                    events.push(Event::Html(
                        format!(
                            "<figure class=diagram><pre class=diagram-src>{}</pre></figure>",
                            esc(&text)
                        )
                        .into(),
                    ));
                }
                Event::Text(t) => text.push_str(&t),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(Tag::CodeBlock(ref kind)) => {
                if let Some(spec) = markdown::quote_spec(kind) {
                    quoting = Some(spec);
                    continue;
                }
                // A mermaid fence goes to the page as its source, which the
                // vendored Pele draws in place; without script it stays code.
                if let pulldown_cmark::CodeBlockKind::Fenced(info) = kind
                    && info.split_whitespace().next() == Some("mermaid")
                {
                    diagram = Some(String::new());
                    continue;
                }
                if let pulldown_cmark::CodeBlockKind::Fenced(info) = kind
                    && let Some((need, id)) = need_fence(info)
                {
                    asks += 1;
                    asking = Some((need, need_id(need, id, asks), String::new()));
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
            // An image recto stored when the document was saved loads from
            // this server, and opens full size in a tab of its own.
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) if stored_image(&dest_url).is_some() => {
                let src = stored_image(&dest_url).unwrap_or_default();
                events.push(Event::Html(
                    format!("<a class=shot href=\"{src}\" target=_blank rel=noopener>").into(),
                ));
                events.push(Event::Start(Tag::Image {
                    link_type,
                    dest_url: src.into(),
                    title,
                    id,
                }));
                images.push(true);
            }
            Event::End(TagEnd::Image) if images.pop() == Some(true) => {
                events.push(Event::End(TagEnd::Image));
                events.push(Event::Html("</a>".into()));
            }
            // A link keeps its text, but only an http(s), mailto or in-page
            // target. Any other image never loads (that would fetch on open);
            // it becomes a link to itself.
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
                images.push(false);
            }
            Event::End(TagEnd::Image) => events.push(Event::End(TagEnd::Link)),
            other => events.push(other),
        }
    }
    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, autolink(events.into_iter()).into_iter());

    let _ = write!(
        out,
        "<article class={class}><div class=tour-label>{}</div>",
        esc(label)
    );
    // The briefing is short and read top to bottom; an outline is noise.
    if outline.len() > 1 && class != "brief" {
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
/// A question the person answers in place, or an action of theirs they mark
/// done. For a question, lines starting `- ` are choices and the rest is the
/// question; an action's only choice is "done". The page sends the answer as
/// a comment anchored to the block's id, so answering is the same gesture as
/// commenting, and either kind keeps the briefing open until it's settled.
fn ask_html(kind: Need, id: &str, text: &str, rig: Option<&str>) -> String {
    let mut question = Vec::new();
    let mut choices = Vec::new();
    for line in text.lines() {
        match line.trim_start().strip_prefix("- ") {
            Some(choice) if kind == Need::Ask => choices.push(choice.trim()),
            _ => question.push(line),
        }
    }
    if kind == Need::Do {
        choices = vec!["done"];
    }
    let question = question.join("\n");
    let mut out = format!(
        "<div class=\"ask{}\" data-ask=\"{}\"",
        if kind == Need::Do { " do" } else { "" },
        esc(id)
    );
    if let Some(rig) = rig {
        let _ = write!(out, " data-rig=\"{}\"", esc(rig));
    }
    out.push('>');
    if kind == Need::Do {
        out.push_str("<div class=ask-kind>for you to do</div>");
    }
    out.push_str("<div class=ask-q>");
    for para in question
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let _ = write!(out, "<p>{}</p>", esc(para).replace('\n', "<br>"));
    }
    out.push_str("</div>");
    if !choices.is_empty() {
        out.push_str("<div class=ask-choices>");
        for c in choices {
            let _ = write!(
                out,
                "<button type=button data-choice=\"{0}\">{0}</button>",
                esc(c)
            );
        }
        out.push_str("</div>");
    }
    out.push_str("<div class=ask-answer></div></div>");
    out
}

/// A quote whose lines changed after it was written: the caption, and a
/// plain statement in place of code that no longer says what it said.
fn outdated_quote(caption: &str, out: &mut String) {
    let _ = write!(
        out,
        "<figure class=quote><figcaption>{} <span class=outdated>outdated</span></figcaption><p class=dim>these lines changed after this was written</p></figure>",
        esc(caption)
    );
}

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
        // Another rig, which the page asks its frame to put on screen.
        Some("rig")
            if url.trim()[4..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) =>
        {
            url
        }
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
/// Pele draws mermaid fences in the page. It's vendored from npm (pele 1.13.0,
/// MIT, `serve/vendor/pele.LICENSE`) and served from here, so a diagram never
/// fetches anything; the version is in the path so the browser can keep it.
const PELE: &str = include_str!("serve/vendor/pele.js");
const PELE_PATH: &str = "/pele-1.13.0.js";

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

    /// a → b [first] → c → d [second] → e, then an empty @: three layers.
    fn layered_repo() -> (
        crate::testing::TempRepo,
        std::collections::HashMap<&'static str, String>,
    ) {
        let repo = crate::testing::TempRepo::new("serve-layers");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        let mut changes = std::collections::HashMap::new();
        for (name, bookmark) in [
            ("a", None),
            ("b", Some("first")),
            ("c", None),
            ("d", Some("second")),
            ("e", None),
        ] {
            std::fs::write(repo.0.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            repo.run("jj", &["describe", "-m", name]);
            if let Some(b) = bookmark {
                repo.run("jj", &["bookmark", "create", b, "-r", "@"]);
            }
            changes.insert(
                name,
                repo.out("jj", &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
            );
            repo.run("jj", &["new"]);
        }
        (repo, changes)
    }

    /// The stack shows its layers, a layer view diffs just that layer, and a
    /// change view steps to its neighbours in reading order, naming the
    /// layer it sits in.
    #[test]
    fn the_window_shows_a_stack_of_layers_and_steps_through_its_changes() {
        let (repo, c) = layered_repo();
        let root = repo.0.to_string_lossy().to_string();

        let (whole, _) = workspace(0, &root, None, None, usize::MAX);
        assert!(whole.contains("<b>whole stack</b>"), "{whole}");
        assert!(whole.contains("5 commits in 3 layers"), "{whole}");
        for layer in ["second", "first", "unpushed"] {
            assert!(
                whole.contains(&format!("<span class=rev-id>{layer}</span>")),
                "{layer}: {whole}"
            );
        }
        assert!(
            whole.contains(&format!(
                "data-view=\"0:{}\" aria-label=\"newer change\"",
                c["a"]
            )),
            "from the whole stack, stepping starts at the oldest change: {whole}"
        );

        let (layer, _) = workspace(
            0,
            &root,
            None,
            Some(&format!("layer:{}", c["d"])),
            usize::MAX,
        );
        assert!(layer.contains("› <b>second</b>"), "{layer}");
        assert!(
            layer.contains("data-path=\"c.txt\"") && layer.contains("data-path=\"d.txt\""),
            "{layer}"
        );
        assert!(
            !layer.contains("data-path=\"b.txt\"") && !layer.contains("data-path=\"e.txt\""),
            "only the layer's own files: {layer}"
        );

        let (change, _) = workspace(0, &root, None, Some(&c["c"][..6]), usize::MAX);
        assert!(
            change.contains(&format!(
                "data-view=\"0:layer:{}\">second</button> › <code>",
                c["d"]
            )),
            "{change}"
        );
        assert!(
            change.contains(&format!(
                "data-view=\"0:{}\" aria-label=\"older change\"",
                c["b"]
            )),
            "{change}"
        );
        assert!(
            change.contains(&format!(
                "data-view=\"0:{}\" aria-label=\"newer change\"",
                c["d"]
            )),
            "{change}"
        );

        let (newest, _) = workspace(0, &root, None, Some(&c["e"]), usize::MAX);
        assert!(
            newest.contains("data-view=\"0:\" aria-label=\"newer change\""),
            "stepping past the newest change comes back to the whole stack: {newest}"
        );
    }

    /// In a stack of PRs each layer goes by the PR it carries, found by its
    /// head commit, and a layer without one keeps its bookmark's name.
    #[test]
    fn a_layer_carrying_a_pr_goes_by_the_pr() {
        let (repo, c) = layered_repo();
        let backend = crate::backend::JjBackend::new(repo.0.clone());
        let commits = backend.commits("root()", "@").expect("commits");
        let head_of = |name: &str| {
            commits
                .iter()
                .find(|commit| commit.change == c[name])
                .unwrap()
                .id
                .clone()
        };
        let pr = |number: u64, title: &str, head: String| link::PullRequest {
            number,
            title: title.into(),
            head_oid: head.to_uppercase(),
            ..crate::testing::empty_pull_request("base")
        };
        let prs = vec![
            pr(41, "the first layer", head_of("b")),
            pr(42, "the second layer", head_of("d")),
        ];
        let layers = crate::stack::layers(&commits);
        assert_eq!(layer_pr(&prs, &layers[1]).map(|p| p.number), Some(42));
        assert_eq!(layer_pr(&prs, &layers[2]).map(|p| p.number), Some(41));
        assert!(
            layer_pr(&prs, &layers[0]).is_none(),
            "unpushed work carries no PR"
        );

        let mut out = String::new();
        render_stack(
            0,
            &commits,
            &crate::stack::View::Stack,
            &StackCtx {
                prs: &prs,
                head: None,
                from_start: false,
                totals: (0, 0),
            },
            &mut out,
        );
        assert!(
            out.contains("<span class=rev-id>#42 the second layer</span>"),
            "{out}"
        );
        assert!(
            out.contains("<span class=rev-id>#41 the first layer</span>"),
            "{out}"
        );
        assert!(out.contains("<span class=rev-id>unpushed</span>"), "{out}");
    }

    /// Moving one repo's view asks for that section alone, and a section
    /// already rendered for the repo's current state comes back from the
    /// cache, until a working-copy edit moves jj's operation on.
    #[test]
    fn a_section_renders_alone_and_is_cached_until_the_repo_moves() {
        let (repo, c) = layered_repo();
        let root = repo.0.to_string_lossy().to_string();
        let req = |only| Request {
            path: "/fragment".into(),
            roots: vec![root.clone(), root.clone()],
            group: None,
            label: None,
            starts: Vec::new(),
            views: vec![(1, c["c"].clone())],
            only,
            peers: Vec::new(),
        };
        let alone = fragment(&req(Some(1)));
        assert!(
            alone.starts_with("<section class=repo id=\"repo-1\""),
            "{alone}"
        );
        assert_eq!(alone.matches("<section class=repo").count(), 1);
        assert!(
            alone.contains("<code>"),
            "the section carries its own view: {alone}"
        );
        assert_eq!(fragment(&req(Some(7))), "", "no such repo, nothing to show");

        let key = || section_key(1, &root, None, Some(&c["c"]), 100).expect("a jj repo has a key");
        let before = key();
        assert_eq!(before, key(), "the same state gives the same key");
        std::fs::write(repo.0.join("f.txt"), "an edit\n").unwrap();
        assert_ne!(before, key(), "a working-copy edit moves the key");
    }

    /// A bare URL anywhere in prose becomes a link, as on GitHub, without
    /// swallowing the punctuation that ends its sentence. Code, existing
    /// links and unsafe schemes are left as they were.
    #[test]
    fn bare_urls_in_prose_become_links() {
        let html = small_markdown(
            "Open http://foxtrotbase:7745. Or (see https://example.com/a_(b)), \
             not `https://in.code` and [named](https://named.example).\n\n\
             ```\nhttps://in.a.block\n```\n\njavascript:alert(1)",
        );
        assert!(
            html.contains(r#"<a href="http://foxtrotbase:7745">http://foxtrotbase:7745</a>."#),
            "{html}"
        );
        assert!(
            html.contains(r#"<a href="https://example.com/a_(b)">https://example.com/a_(b)</a>)"#),
            "{html}"
        );
        assert!(html.contains("<code>https://in.code</code>"), "{html}");
        assert!(
            html.contains(r#"<a href="https://named.example">named</a>"#),
            "{html}"
        );
        assert!(
            !html.contains(r#"href="https://named.example">https"#),
            "{html}"
        );
        assert!(html.contains("https://in.a.block\n</code>"), "{html}");
        assert!(!html.contains("href=\"javascript"), "{html}");

        let mut out = String::new();
        render_doc(
            "brief",
            "bg",
            "Briefing",
            "## Needs you\n\nThe demo is at http://foxtrotbase:7745 now.",
            &mut 0,
            &mut out,
            &mut |_, _, _| {},
        );
        assert!(
            out.contains(r#"<a href="http://foxtrotbase:7745">"#),
            "{out}"
        );
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
        render_group_doc(
            &GroupQuotes {
                group,
                rendered: &rendered,
                quoted: &HashMap::new(),
            },
            "tour",
            "tour",
            "# Across\n\n```recto beta/src/a.rs:2\n```\n\n```recto gamma/x.rs:1\n```\n",
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
    fn doc_names_read_as_titles() {
        assert_eq!(doc_title("tour"), "Tour");
        assert_eq!(doc_title("pr-body"), "Pr body");
    }

    #[test]
    fn ask_blocks_become_cards_with_choices() {
        let mut out = String::new();
        render_doc(
            "brief",
            "bg",
            "Briefing",
            "## Needs you\n\n```ask q1\nFold the list <by default>?\n- folded\n- open\n```\n\n```ask\nAnything else?\n```\n",
            &mut 0,
            &mut out,
            &mut |_, _, _| {},
        );
        assert!(out.contains("<article class=brief>"), "{out}");
        assert!(out.contains("data-ask=\"q1\""), "{out}");
        assert!(out.contains("Fold the list &lt;by default&gt;?"), "{out}");
        assert!(out.contains("data-choice=\"open\""), "{out}");
        // An ask without an id gets one by position.
        assert!(out.contains("data-ask=\"ask2\""), "{out}");
    }

    /// An action that's the person's to take is a `do` block: a card of its
    /// own kind whose one choice is done, counted with the asks so the
    /// briefing stays open and a coordinator gathers it. A dash line in it is
    /// part of the action, not a choice.
    #[test]
    fn do_blocks_are_actions_marked_done() {
        let mut out = String::new();
        render_doc(
            "brief",
            "bg",
            "Briefing",
            "## Needs you\n\n```do mac-install\nClick Install on the Mac.\n- it may ask for Touch ID\n```\n\n```do\nTry the dev pill.\n```\n\n```dog\nnot a do\n```\n",
            &mut 0,
            &mut out,
            &mut |_, _, _| {},
        );
        assert!(
            out.contains("class=\"ask do\" data-ask=\"mac-install\""),
            "{out}"
        );
        assert!(out.contains("for you to do"), "{out}");
        assert!(out.contains("data-choice=\"done\""), "{out}");
        assert!(
            !out.contains("data-choice=\"it may ask"),
            "dash lines are part of the action: {out}"
        );
        assert!(
            out.contains("data-ask=\"do2\""),
            "an unnamed do is numbered: {out}"
        );
        assert!(!out.contains("data-ask=\"dog"), "{out}");

        let gathered =
            brief_asks("```do mac-install\nClick Install.\n```\n\n```ask q\nShip?\n```\n");
        let kinds: Vec<bool> = gathered.iter().map(|(k, _, _)| *k == Need::Do).collect();
        assert_eq!(
            kinds,
            [true, false],
            "a coordinator gathers to-dos with the asks"
        );
    }

    // A coordinator gathers another rig's asks by the ids its own page uses,
    // so an answer from either page marks the same ask.
    #[test]
    fn gathered_asks_keep_the_ids_their_own_page_gives_them() {
        let asks = brief_asks(
            "```ask q1\nShip?\n- yes\n```\n\ntext\n\n```ask\nAnything else?\n```\n```rust\nfn x() {}\n```\n",
        );
        let ids: Vec<&str> = asks.iter().map(|(_, id, _)| id.as_str()).collect();
        assert_eq!(ids, ["q1", "ask2"]);
        assert_eq!(asks[0].2, "Ship?\n- yes\n");
        let card = ask_html(Need::Ask, "q1", &asks[0].2, Some("other-rig"));
        assert!(
            card.contains("data-ask=\"q1\" data-rig=\"other-rig\""),
            "{card}"
        );
    }

    #[test]
    fn gathered_asks_lead_the_briefing() {
        let mut out = String::new();
        brief_with(
            "<section class=gathered></section>",
            "<details class=group-brief open><summary class=brief-strip>Briefing</summary><article>own</article></details>",
            &mut out,
        );
        assert_eq!(
            out,
            "<details class=group-brief open><summary class=brief-strip>Briefing</summary><section class=gathered></section><article>own</article></details>"
        );
        // A coordinator that hasn't written a briefing still gets one to hold them.
        let mut out = String::new();
        brief_with("<section class=gathered></section>", "", &mut out);
        assert!(
            out.starts_with("<details class=group-brief open>") && out.contains("gathered"),
            "{out}"
        );
        // Nothing gathered leaves the briefing as it was.
        let mut out = String::new();
        brief_with("", "<p>own</p>", &mut out);
        assert_eq!(out, "<p>own</p>");
    }

    #[test]
    fn a_rig_link_survives_but_only_naming_a_rig() {
        assert_eq!(
            &*safe_url("rig:cos-2026-11-12".into()),
            "rig:cos-2026-11-12"
        );
        assert_eq!(&*safe_url("rig:../../etc".into()), "#");
        assert_eq!(&*safe_url("rig:a b".into()), "#");
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
    fn mermaid_fences_go_to_the_page_as_escaped_source() {
        let mut out = String::new();
        render_tour(
            "t0",
            "Tour",
            "```mermaid\nflowchart LR\n  A[<b>x</b>] --> B\n```\n\n```mermaidish\nplain\n```\n",
            &mut 0,
            &mut out,
            &mut |_, _, _| {},
        );
        assert!(
            out.contains("<figure class=diagram><pre class=diagram-src>flowchart LR\n  A[&lt;b&gt;x&lt;/b&gt;] --&gt; B\n</pre></figure>"),
            "{out}"
        );
        assert_eq!(out.matches("class=diagram>").count(), 1, "{out}");
    }

    #[test]
    fn stored_images_load_from_this_server_and_nothing_else_does() {
        let hash = "c".repeat(64);
        let mut out = String::new();
        render_tour(
            "t0",
            "Tour",
            &format!(
                "![shot](recto-asset:{hash}.png) ![forged](recto-asset:../x.png) ![outer ![inner](https://t/i.png)](recto-asset:{hash}.svg)"
            ),
            &mut 0,
            &mut out,
            &mut |_, _, _| {},
        );
        assert!(
            out.contains(&format!(
                "<a class=shot href=\"asset/{hash}.png\" target=_blank rel=noopener><img src=\"asset/{hash}.png\" alt=\"shot\" /></a>"
            )),
            "{out}"
        );
        assert!(!out.contains("src=\"recto-asset"), "{out}");
        assert!(!out.contains("src=\"https"), "{out}");
        assert_eq!(out.matches("<img").count(), 2, "{out}");
        assert!(
            out.contains("alt=\"outer image: inner\" /></a></p>"),
            "{out}"
        );
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
