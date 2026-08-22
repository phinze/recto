//! Live-reload watch registration with repository-aware pruning.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use notify::{Event, EventKind, RecursiveMode, Watcher};

pub fn is_interesting(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

pub fn may_add_directories(event: &Event) -> bool {
    matches!(event.kind, EventKind::Create(_))
}

/// Non-recursive watches let us honor ignore files and prune metadata trees.
/// Refreshing after a create event discovers directories added after startup.
pub struct WatchTree {
    root: PathBuf,
    registered: HashSet<PathBuf>,
}

impl WatchTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            registered: HashSet::new(),
        }
    }

    pub fn refresh(&mut self, watcher: &mut impl Watcher) {
        for dir in watched_dirs(&self.root) {
            if self.registered.contains(&dir) {
                continue;
            }
            // One bad directory (permission, ENOSPC) should not take down all
            // live reload. Failed paths stay absent so a later refresh retries.
            if watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                self.registered.insert(dir);
            }
        }
    }
}

/// Every source directory under `root`, honoring ignore files while retaining
/// tracked dotted directories such as `.github` and `.cargo`.
fn watched_dirs(root: &Path) -> Vec<PathBuf> {
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    // `!` inverts gitignore sense in an Override. VCS metadata is not normally
    // listed in ignore files, and `.direnv` gets an explicit second guard.
    for dir in [".git", ".jj", ".direnv"] {
        overrides
            .add(&format!("!{dir}/"))
            .expect("static override glob is valid");
    }
    let overrides = overrides.build().expect("static overrides build");

    ignore::WalkBuilder::new(root)
        .follow_links(false)
        .hidden(false)
        .overrides(overrides)
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_dir()))
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(dirs: &[&str]) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!("recto-watched-dirs-{nonce}"));
            for dir in dirs {
                std::fs::create_dir_all(root.join(dir)).expect("create temp subtree");
            }
            Self(root)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dotted_content_stays_but_metadata_is_pruned() {
        let tree = TempTree::new(&[
            "src",
            ".github/workflows",
            ".git/objects",
            ".jj",
            ".direnv/flake-inputs",
        ]);
        let root = &tree.0;

        let watched: HashSet<PathBuf> = watched_dirs(root)
            .into_iter()
            .map(|p| p.strip_prefix(root).unwrap_or(&p).to_path_buf())
            .collect();

        assert!(watched.contains(Path::new("src")), "watched = {watched:?}");
        assert!(
            watched.contains(Path::new(".github/workflows")),
            "watched = {watched:?}"
        );
        for pruned in [".git", ".git/objects", ".jj", ".direnv"] {
            assert!(
                !watched.contains(Path::new(pruned)),
                "{pruned} should be pruned; watched = {watched:?}"
            );
        }
    }

    #[test]
    fn rescanning_finds_a_directory_created_after_startup() {
        let tree = TempTree::new(&["src"]);
        assert!(!watched_dirs(&tree.0).contains(&tree.0.join("generated")));

        std::fs::create_dir(tree.0.join("generated")).unwrap();

        assert!(watched_dirs(&tree.0).contains(&tree.0.join("generated")));
    }
}
