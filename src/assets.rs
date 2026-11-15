//! Images an agent's Markdown shows on the web page.
//!
//! A briefing, doc or tour names an image the ordinary way, `![alt](shot.png)`,
//! with a path on this machine. When the document is saved, recto copies each
//! such file into one content-addressed store beside the workspace states and
//! rewrites the reference to `recto-asset:HASH.EXT`. `recto serve` hands those
//! out by name and never resolves a path from a request, so a page can show an
//! image from the agent's scratchpad without opening the filesystem to the
//! browser, and a regenerated screenshot doesn't change a briefing until the
//! agent writes it again.
//!
//! The store holds whatever some workspace's state still names. Saving a
//! document and forgetting a workspace both sweep the rest, so images live as
//! long as the documents that show them.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use pulldown_cmark::{Event, Options, Parser, Tag};
use sha2::{Digest, Sha256};

/// The URL scheme a stored image goes by in saved Markdown.
pub(crate) const SCHEME: &str = "recto-asset:";

/// Past this, an image is more likely a mistake than a screenshot.
const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// A sweep spares anything this fresh. A per-repo tour is copied in by the
/// CLI and saved by the viewer a moment later, and an unrelated save landing
/// between the two mustn't take its image.
const GRACE: Duration = Duration::from_secs(60 * 60);

const TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("svg", "image/svg+xml"),
];

pub(crate) fn store_dir() -> Result<PathBuf> {
    Ok(crate::state::state_home()?.join("recto/assets"))
}

/// The content type for a stored image's name, which is also the check that
/// the name is one recto could have written: a SHA-256 in hex and a known
/// extension, so nothing in it can climb out of the store.
pub(crate) fn content_type(name: &str) -> Option<&'static str> {
    let (hash, ext) = name.split_once('.')?;
    if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    type_for_ext(ext)
}

fn type_for_ext(ext: &str) -> Option<&'static str> {
    TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t)
}

/// Copy every local image `body` names into the store and point the body at
/// the copies. Relative paths resolve against `base`. Remote and already
/// stored images are left as they are; a local path that can't be stored is
/// an error naming it, since a briefing with a silently broken picture is
/// worse than a refused write.
pub(crate) fn ingest(body: &str, base: &Path) -> Result<String> {
    ingest_into(&store_dir()?, body, base)
}

fn ingest_into(store: &Path, body: &str, base: &Path) -> Result<String> {
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for (event, range) in Parser::new_ext(body, Options::all()).into_offset_iter() {
        let Event::Start(Tag::Image { dest_url, .. }) = event else {
            continue;
        };
        if !is_local(&dest_url) {
            continue;
        }
        // The event's range spans the whole `![alt](dest "title")`; the
        // destination is the text after the last `](` that matches it.
        let text = &body[range.clone()];
        let at = text
            .rfind("](")
            .and_then(|open| text[open..].find(&*dest_url).map(|i| open + i))
            .ok_or_else(|| {
                anyhow!("image {dest_url}: write it inline, as ![alt]({dest_url}), to show it")
            })?;
        let name = store_file(store, &base.join(&*dest_url))
            .with_context(|| format!("image {dest_url}"))?;
        let start = range.start + at;
        edits.push((start..start + dest_url.len(), format!("{SCHEME}{name}")));
    }
    let mut out = body.to_string();
    for (range, replacement) in edits.into_iter().rev() {
        out.replace_range(range, &replacement);
    }
    Ok(out)
}

/// A path on this machine, as opposed to a URL or an in-page anchor.
fn is_local(dest: &str) -> bool {
    !dest.is_empty() && !dest.starts_with('#') && !dest.contains(':')
}

fn store_file(store: &Path, path: &Path) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| type_for_ext(e).is_some())
        .ok_or_else(|| {
            let known: Vec<&str> = TYPES.iter().map(|(e, _)| *e).collect();
            anyhow!("only {} images can be shown", known.join(", "))
        })?;
    let meta = fs::metadata(path).with_context(|| format!("could not read {}", path.display()))?;
    if !meta.is_file() {
        return Err(anyhow!("{} is not a file", path.display()));
    }
    if meta.len() > MAX_BYTES {
        return Err(anyhow!(
            "{} is {} MB; images over {} MB aren't stored",
            path.display(),
            meta.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        ));
    }
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let name = format!("{:x}.{ext}", Sha256::digest(&bytes));
    let dest = store.join(&name);
    if dest.exists() {
        // Touch it, so a sweep counts it as fresh until the document that
        // names it again has been saved.
        let _ = fs::File::options()
            .append(true)
            .open(&dest)
            .and_then(|f| f.set_modified(SystemTime::now()));
    } else {
        fs::create_dir_all(store)
            .with_context(|| format!("could not create {}", store.display()))?;
        let temporary = store.join(format!(".{name}.{}", std::process::id()));
        fs::write(&temporary, &bytes)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        fs::rename(&temporary, &dest)
            .with_context(|| format!("could not store {}", dest.display()))?;
    }
    Ok(name)
}

/// Remove stored images that no workspace's state names any more.
pub(crate) fn sweep() -> Result<()> {
    sweep_in(&crate::state::state_home()?)
}

pub(crate) fn sweep_in(state_home: &Path) -> Result<()> {
    sweep_at(
        &state_home.join("recto/assets"),
        &state_home.join("recto/workspaces"),
        GRACE,
    )
}

fn sweep_at(store: &Path, workspaces: &Path, grace: Duration) -> Result<()> {
    let Ok(entries) = fs::read_dir(store) else {
        return Ok(());
    };
    // Reading the raw JSON is enough: a stored name is distinctive, and a
    // false keep costs one leftover file until the next sweep.
    let mut named = String::new();
    if let Ok(states) = fs::read_dir(workspaces) {
        for state in states.flatten() {
            if state.path().extension().is_some_and(|e| e == "json")
                && let Ok(text) = fs::read_to_string(state.path())
            {
                named.push_str(&text);
            }
        }
    }
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let fresh = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() < grace);
        if fresh || named.contains(&format!("{SCHEME}{name}")) {
            continue;
        }
        let _ = fs::remove_file(entry.path());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recto-assets-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn local_images_are_copied_and_rewritten() {
        let root = scratch("ingest");
        let store = root.join("store");
        fs::write(root.join("shot.png"), b"not really a png").unwrap();
        let body = "Look:\n\n![the shot](shot.png \"title\")\n\nand ![remote](https://x.test/a.png) and [shot.png](shot.png)\n";
        let out = ingest_into(&store, body, &root).unwrap();
        let name = format!("{:x}.png", Sha256::digest(b"not really a png"));
        assert!(
            out.contains(&format!("![the shot]({SCHEME}{name} \"title\")")),
            "{out}"
        );
        assert!(out.contains("![remote](https://x.test/a.png)"), "{out}");
        // A link to the file is a link, not an image.
        assert!(out.contains("[shot.png](shot.png)"), "{out}");
        assert_eq!(fs::read(store.join(&name)).unwrap(), b"not really a png");
        assert_eq!(content_type(&name), Some("image/png"));
        // Saving again is a no-op.
        assert_eq!(ingest_into(&store, &out, &root).unwrap(), out);
    }

    #[test]
    fn unusable_images_are_refused_by_name() {
        let root = scratch("refuse");
        let store = root.join("store");
        fs::write(root.join("notes.txt"), b"text").unwrap();
        let err = ingest_into(&store, "![x](missing.png)", &root).unwrap_err();
        assert!(format!("{err:#}").contains("missing.png"), "{err:#}");
        let err = ingest_into(&store, "![x](notes.txt)", &root).unwrap_err();
        assert!(format!("{err:#}").contains("only png"), "{err:#}");
        let err = ingest_into(&store, "![x][r]\n\n[r]: notes.png\n", &root).unwrap_err();
        assert!(format!("{err:#}").contains("inline"), "{err:#}");
    }

    #[test]
    fn names_from_a_request_cant_leave_the_store() {
        let hash = "a".repeat(64);
        assert_eq!(content_type(&format!("{hash}.svg")), Some("image/svg+xml"));
        assert_eq!(content_type(&format!("{hash}.html")), None);
        assert_eq!(content_type("../../etc/passwd.png"), None);
        assert_eq!(content_type(&format!("{}.png", "A".repeat(64))), None);
        assert_eq!(content_type(&format!("../{}.png", &hash[3..])), None);
    }

    #[test]
    fn a_sweep_keeps_named_and_fresh_images() {
        let root = scratch("sweep");
        let (store, workspaces) = (root.join("store"), root.join("workspaces"));
        fs::create_dir_all(&store).unwrap();
        fs::create_dir_all(&workspaces).unwrap();
        let (kept, dropped) = (
            format!("{}.png", "1".repeat(64)),
            format!("{}.png", "2".repeat(64)),
        );
        fs::write(store.join(&kept), b"k").unwrap();
        fs::write(store.join(&dropped), b"d").unwrap();
        fs::write(
            workspaces.join("w.json"),
            format!("{{\"brief\":\"![a]({SCHEME}{kept})\"}}"),
        )
        .unwrap();
        sweep_at(&store, &workspaces, Duration::from_secs(3600)).unwrap();
        assert!(store.join(&dropped).exists(), "fresh files survive");
        sweep_at(&store, &workspaces, Duration::ZERO).unwrap();
        assert!(store.join(&kept).exists());
        assert!(!store.join(&dropped).exists());
    }
}
