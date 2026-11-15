//! Anchors that outlive a rewrite.
//!
//! Everything Recto pins to code (notes, review drafts, focus, tour stops)
//! used to be a path and a line range in whatever diff was loaded, so it meant
//! different lines in different views and slid or vanished when a commit was
//! amended. A [`Stamp`] instead names the change whose tree the lines live in,
//! the commit that change was when the stamp was taken, and the lines in that
//! commit's tree. Resolving one for display is at most two line maps: from the
//! stamped commit to the change's current commit (a rewrite), then from there
//! to the head of the view (a wider lens). Lines that themselves changed make
//! the anchor *outdated*, which shows rather than silently disappearing.
//!
//! Git repositories don't stamp yet: `annotate` declines there, so their anchors keep reading lines against the view.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::backend::Backend;

/// Where an anchor's lines live: a change, the commit it was when stamped,
/// and the lines in that commit's tree. The snippet is those lines' text, the
/// last resort when the stamped commit can no longer be read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub change: String,
    pub commit: String,
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippet: Vec<String>,
}

/// The durable identity of an anchored object, beside the path and lines it
/// carries for the current view. Without a stamp (a git repository, or state
/// written before stamps existed) the object's lines are read against the
/// current view, which is how every anchor used to work.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<Stamp>,
    /// The anchored lines changed after the stamp was taken.
    #[serde(default, skip_serializing_if = "is_false")]
    pub outdated: bool,
    /// The anchor belongs to a change the current narrowed view doesn't show.
    #[serde(skip)]
    pub elsewhere: bool,
}

impl Pin {
    pub fn stamped(stamp: Option<Stamp>) -> Self {
        Self {
            stamp,
            ..Self::default()
        }
    }

    /// Whether the object's lines can be drawn in the current view.
    pub fn shown(&self) -> bool {
        !self.outdated && !self.elsewhere
    }

    /// Fold a resolution into the object's pin and current lines. Returns
    /// whether anything durable changed, so the caller knows to save.
    pub fn apply(&mut self, resolution: &Resolution, start: &mut u32, end: &mut u32) -> bool {
        let before = (self.stamp.clone(), self.outdated, *start, *end);
        match resolution {
            Resolution::At {
                start: s,
                end: e,
                stamp,
            } => {
                *start = *s;
                *end = *e;
                self.stamp = Some(stamp.clone());
                self.outdated = false;
                self.elsewhere = false;
            }
            Resolution::Outdated => {
                self.outdated = true;
                self.elsewhere = false;
            }
            Resolution::Elsewhere => self.elsewhere = true,
        }
        before != (self.stamp.clone(), self.outdated, *start, *end)
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

/// What the diff pane is showing. A range lens reads files at the head of the
/// range and shows every anchor that maps into it. A narrowed lens reads them
/// at its head and shows only the anchors of its own commits: one change, or
/// the commits of one layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lens {
    pub head: String,
    /// The commits a narrowed lens shows; `None` for a range.
    pub members: Option<Vec<String>>,
}

impl Lens {
    pub fn range(backend: &dyn Backend) -> Self {
        Self {
            head: backend.head_rev().to_string(),
            members: None,
        }
    }

    pub fn narrowed(commit: String) -> Self {
        Self {
            members: Some(vec![commit.clone()]),
            head: commit,
        }
    }

    /// One layer: read at its head, showing its commits' anchors.
    pub fn layer(head: String, commits: Vec<String>) -> Self {
        Self {
            head,
            members: Some(commits),
        }
    }

    pub fn is_narrowed(&self) -> bool {
        self.members.is_some()
    }

    fn shows(&self, commit: &str) -> bool {
        self.members
            .as_ref()
            .is_none_or(|m| m.iter().any(|c| c == commit))
    }
}

/// Where a stamp lands in a lens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Its lines in the lens head, and the stamp refreshed to the change's
    /// current commit so later resolutions start from there.
    At { start: u32, end: u32, stamp: Stamp },
    /// The anchored lines changed since the stamp was taken.
    Outdated,
    /// A narrowed lens on some other change.
    Elsewhere,
}

/// Stamps and resolves anchors against one backend, caching file contents
/// and change lookups for the life of one load.
pub struct Resolver<'a> {
    backend: &'a dyn Backend,
    texts: HashMap<(String, String), Option<Vec<String>>>,
    commits: HashMap<String, Option<String>>,
}

impl<'a> Resolver<'a> {
    pub fn new(backend: &'a dyn Backend) -> Self {
        Self {
            backend,
            texts: HashMap::new(),
            commits: HashMap::new(),
        }
    }

    /// Look up the current commit of many changes in one backend call.
    pub fn prefetch<'s>(&mut self, changes: impl IntoIterator<Item = &'s str>) {
        let wanted: Vec<String> = changes
            .into_iter()
            .filter(|c| !self.commits.contains_key(*c))
            .map(str::to_string)
            .collect();
        if wanted.is_empty() {
            return;
        }
        let found = self.backend.change_commits(&wanted);
        for change in wanted {
            let commit = found.get(&change).cloned();
            self.commits.insert(change, commit);
        }
    }

    fn current(&mut self, change: &str) -> Option<String> {
        self.prefetch([change]);
        self.commits.get(change).cloned().flatten()
    }

    fn text(&mut self, rev: &str, path: &str) -> Option<Vec<String>> {
        let key = (rev.to_string(), path.to_string());
        if let Some(text) = self.texts.get(&key) {
            return text.clone();
        }
        let text = self
            .backend
            .file_content(rev, path)
            .ok()
            .map(|t| t.lines().map(str::to_string).collect::<Vec<_>>());
        self.texts.insert(key, text.clone());
        text
    }

    /// Stamp lines `start..=end` of `path` as the lens shows them. The owner
    /// is the newest change that touched any of those lines, which is the
    /// one tree every line of the span exists in. `None` when the backend
    /// can't say who owns lines (git, for now), which leaves the object
    /// reading its lines against the view as before.
    pub fn stamp(&mut self, lens: &Lens, path: &str, start: u32, end: u32) -> Option<Stamp> {
        let (start, end) = (start.min(end), start.max(end));
        let owners = self.backend.annotate(&lens.head, path)?;
        let span = owners.get(start.checked_sub(1)? as usize..end as usize)?;
        let mut changes: Vec<String> = Vec::new();
        for owner in span {
            if !changes.contains(owner) {
                changes.push(owner.clone());
            }
        }
        let change = match changes.as_slice() {
            [one] => one.clone(),
            _ => self.backend.newest_change(&changes)?,
        };
        let commit = self.current(&change)?;
        let head = self.text(&lens.head, path)?;
        let owned = self.text(&commit, path)?;
        let (s, e) = map_span(&head, &owned, start, end)?;
        Some(Stamp {
            snippet: owned[s as usize - 1..e as usize].to_vec(),
            change,
            commit,
            start: s,
            end: e,
        })
    }

    /// Where `stamp`, on `path`, lands in the lens.
    pub fn resolve(&mut self, lens: &Lens, path: &str, stamp: &Stamp) -> Resolution {
        let Some(current) = self.current(&stamp.change) else {
            // The change is gone (abandoned, squashed away) or divergent, so
            // the snippet is all that's left to find the lines by.
            return match self
                .text(&lens.head, path)
                .and_then(|head| find_snippet(&head, &stamp.snippet))
            {
                Some((start, end)) => Resolution::At {
                    start,
                    end,
                    stamp: stamp.clone(),
                },
                None => Resolution::Outdated,
            };
        };
        if !lens.shows(&current) {
            return Resolution::Elsewhere;
        }
        let Some(now) = self.text(&current, path) else {
            return Resolution::Outdated;
        };
        let moved = if current == stamp.commit {
            Some((stamp.start, stamp.end))
        } else {
            match self.text(&stamp.commit, path) {
                Some(then) => map_span(&then, &now, stamp.start, stamp.end),
                None => find_snippet(&now, &stamp.snippet),
            }
        };
        let Some((s, e)) = moved else {
            return Resolution::Outdated;
        };
        let refreshed = Stamp {
            change: stamp.change.clone(),
            commit: current.clone(),
            start: s,
            end: e,
            snippet: now[s as usize - 1..e as usize].to_vec(),
        };
        let (start, end) = if current == lens.head {
            (s, e)
        } else {
            let Some(head) = self.text(&lens.head, path) else {
                return Resolution::Outdated;
            };
            match map_span(&now, &head, s, e) {
                Some(span) => span,
                None => return Resolution::Outdated,
            }
        };
        Resolution::At {
            start,
            end,
            stamp: refreshed,
        }
    }
}

/// Resolve a batch of `(path, stamp)` pairs against one lens, sharing one
/// change lookup and one file cache across them.
pub fn resolve_all(
    backend: &dyn Backend,
    lens: &Lens,
    stamps: &[(String, Stamp)],
) -> Vec<((String, Stamp), Resolution)> {
    let mut resolver = Resolver::new(backend);
    resolver.prefetch(stamps.iter().map(|(_, s)| s.change.as_str()));
    stamps
        .iter()
        .map(|(path, stamp)| {
            let r = resolver.resolve(lens, path, stamp);
            ((path.clone(), stamp.clone()), r)
        })
        .collect()
}

/// Something pinned to lines of a file, so a batch of them can be settled
/// without knowing what each one is.
pub trait Pinned {
    fn parts(&mut self) -> (&str, &mut Pin, &mut u32, &mut u32);
}

/// Resolve every stamped item against the lens and fold the result into its
/// lines. Unstamped items keep reading their lines against the view. Returns
/// whether anything durable changed.
pub fn settle<T: Pinned>(backend: &dyn Backend, lens: &Lens, items: &mut [T]) -> bool {
    let stamps: Vec<(String, Stamp)> = items
        .iter_mut()
        .filter_map(|item| {
            let (path, pin, _, _) = item.parts();
            Some((path.to_string(), pin.stamp.clone()?))
        })
        .collect();
    let resolved = resolve_all(backend, lens, &stamps);
    let mut changed = false;
    for item in items.iter_mut() {
        let (path, pin, start, end) = item.parts();
        let Some(stamp) = pin.stamp.clone() else {
            continue;
        };
        if let Some((_, r)) = resolved.iter().find(|((p, s), _)| p == path && *s == stamp) {
            changed |= pin.apply(r, start, end);
        }
    }
    changed
}

/// A tour or doc quote's stamp, kept beside the Markdown so the document
/// itself stays exactly what the agent wrote. `spec` is the quote's fence
/// text, which is also how a render finds it; `path` is relative to the
/// repository the quote lives in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteStamp {
    pub spec: String,
    pub path: String,
    pub start: u32,
    pub end: u32,
    #[serde(default, flatten)]
    pub pin: Pin,
}

impl Pinned for QuoteStamp {
    fn parts(&mut self) -> (&str, &mut Pin, &mut u32, &mut u32) {
        (&self.path, &mut self.pin, &mut self.start, &mut self.end)
    }
}

/// Where a quote's lines are now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Quoted {
    /// The spec to lift, with its lines moved to where they sit now.
    At(String),
    /// Its lines changed since the quote was written.
    Outdated,
    /// It belongs to a change the narrowed view isn't showing.
    Elsewhere,
}

impl QuoteStamp {
    pub fn quoted(&self) -> Quoted {
        if self.pin.outdated {
            return Quoted::Outdated;
        }
        if self.pin.elsewhere {
            return Quoted::Elsewhere;
        }
        let (path, _, _) = crate::parse_pathspec(&self.spec);
        Quoted::At(if self.start == self.end {
            format!("{path}:{}", self.start)
        } else {
            format!("{path}:{}-{}", self.start, self.end)
        })
    }
}

/// Every `recto` quote spec in a Markdown document, in order.
pub fn quote_specs(source: &str) -> Vec<String> {
    let mut skip = |_: &str| Vec::new();
    crate::markdown::with_quotes(source, &mut skip)
        .quotes
        .into_iter()
        .map(|(_, spec)| spec.trim().to_string())
        .collect()
}

/// Stamp quotes against a range lens. `located` pairs each spec with its
/// repository-relative path and lines; specs that name no lines, or whose
/// lines the backend can't stamp, are left out and render as they always
/// did.
pub fn stamp_quotes(
    backend: &dyn Backend,
    lens: &Lens,
    located: &[(String, String, u32, u32)],
) -> Vec<QuoteStamp> {
    let mut resolver = Resolver::new(backend);
    located
        .iter()
        .filter_map(|(spec, path, start, end)| {
            let stamp = resolver.stamp(lens, path, *start, *end)?;
            Some(QuoteStamp {
                spec: spec.clone(),
                path: path.clone(),
                start: *start,
                end: *end,
                pin: Pin::stamped(Some(stamp)),
            })
        })
        .collect()
}

/// The specs of one repository's quotes in `source`, located in it.
pub fn locate_quotes(source: &str) -> Vec<(String, String, u32, u32)> {
    quote_specs(source)
        .into_iter()
        .filter_map(|spec| {
            let (path, start, end) = crate::parse_pathspec(&spec);
            let start = start?;
            let end = end.unwrap_or(start).max(start);
            Some((spec.clone(), path.to_string(), start, end))
        })
        .collect()
}

/// The repository a group quote lives in: the first of `repos` (directory
/// names under the group) its path starts with, and the path inside it.
pub fn split_group_path<'p>(repos: &[String], path: &'p str) -> Option<(String, &'p str)> {
    repos.iter().find_map(|repo| {
        path.strip_prefix(repo.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| (repo.clone(), rest))
    })
}

/// Stamp a group document's quotes, each against its own repository.
pub fn stamp_group_quotes(
    group: &std::path::Path,
    repos: &[String],
    source: &str,
) -> Vec<QuoteStamp> {
    let mut by_repo: HashMap<String, Vec<(String, String, u32, u32)>> = HashMap::new();
    for (spec, path, start, end) in locate_quotes(source) {
        if let Some((repo, rest)) = split_group_path(repos, &path) {
            by_repo
                .entry(repo)
                .or_default()
                .push((spec, rest.to_string(), start, end));
        }
    }
    let mut out = Vec::new();
    for (repo, located) in by_repo {
        if let Some(backend) = crate::backend::detect_backend_from(&group.join(&repo)) {
            out.extend(stamp_quotes(
                backend.as_ref(),
                &Lens::range(backend.as_ref()),
                &located,
            ));
        }
    }
    out
}

/// Settle a group's quote stamps, each against its own repository, and say
/// where each spec's lines are now.
pub fn settle_group_quotes(
    group: &std::path::Path,
    repos: &[String],
    quotes: &[QuoteStamp],
) -> HashMap<String, Quoted> {
    let mut by_repo: HashMap<String, Vec<QuoteStamp>> = HashMap::new();
    for q in quotes {
        if let Some((repo, _)) = split_group_path(repos, crate::parse_pathspec(&q.spec).0) {
            by_repo.entry(repo).or_default().push(q.clone());
        }
    }
    let mut out = HashMap::new();
    for (repo, mut quotes) in by_repo {
        if let Some(backend) = crate::backend::detect_backend_from(&group.join(&repo)) {
            settle(
                backend.as_ref(),
                &Lens::range(backend.as_ref()),
                &mut quotes,
            );
            out.extend(quotes.iter().map(|q| (q.spec.clone(), q.quoted())));
        }
    }
    out
}

/// Carry lines `start..=end` (1-based) of `from` across to `to`. Every line
/// of the span has to survive unchanged; one that was edited or removed means
/// the span no longer says what it said, so there is no honest place for it.
pub fn map_span(from: &[String], to: &[String], start: u32, end: u32) -> Option<(u32, u32)> {
    if start == 0 || end < start || end as usize > from.len() {
        return None;
    }
    let ops = capture_diff_slices(Algorithm::Myers, from, to);
    let map = |line: u32| -> Option<u32> {
        let i = line as usize - 1;
        ops.iter().find_map(|op| match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } if (old_index..old_index + len).contains(&i) => {
                Some((new_index + (i - old_index)) as u32 + 1)
            }
            _ => None,
        })
    };
    let mut mapped = Vec::with_capacity((end - start + 1) as usize);
    for line in start..=end {
        mapped.push(map(line)?);
    }
    Some((mapped[0], *mapped.last()?))
}

/// The one place `snippet` appears verbatim in `text`, if it appears exactly
/// once. Two matches are as good as none: guessing would pin a comment to code
/// it was never about.
pub fn find_snippet(text: &[String], snippet: &[String]) -> Option<(u32, u32)> {
    if snippet.is_empty() || snippet.len() > text.len() {
        return None;
    }
    let mut hits = text
        .windows(snippet.len())
        .enumerate()
        .filter(|(_, window)| *window == snippet)
        .map(|(i, _)| i);
    let first = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some((first as u32 + 1, (first + snippet.len()) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::JjBackend;
    use crate::testing::TempRepo;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn a_span_follows_lines_inserted_above_it() {
        let from = lines("a\nb\nc\nd");
        let to = lines("new\nnew\na\nb\nc\nd");
        assert_eq!(map_span(&from, &to, 2, 3), Some((4, 5)));
    }

    #[test]
    fn a_span_whose_own_line_changed_does_not_map() {
        let from = lines("a\nb\nc\nd");
        let to = lines("a\nB\nc\nd");
        assert_eq!(map_span(&from, &to, 2, 3), None);
        assert_eq!(map_span(&from, &to, 3, 4), Some((3, 4)));
    }

    #[test]
    fn a_snippet_matches_only_when_it_is_unique() {
        let text = lines("x\nfn a()\ny\nfn a()\nz\nfn b()");
        assert_eq!(find_snippet(&text, &lines("fn b()")), Some((6, 6)));
        assert_eq!(find_snippet(&text, &lines("fn a()")), None);
        assert_eq!(find_snippet(&text, &lines("y\nfn a()")), Some((3, 4)));
    }

    /// base → A (edits line 10) → B (edits line 15) → @ (empty)
    fn stack() -> (TempRepo, JjBackend, String) {
        let repo = TempRepo::new("anchor-stack");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        std::fs::write(repo.0.join("f.txt"), &body).unwrap();
        repo.run("jj", &["describe", "-m", "base"]);
        repo.run("jj", &["new", "-m", "A"]);
        std::fs::write(
            repo.0.join("f.txt"),
            body.replace("line 10\n", "line 10 by A\n"),
        )
        .unwrap();
        repo.run("jj", &["new", "-m", "B"]);
        let a_and_b = body
            .replace("line 10\n", "line 10 by A\n")
            .replace("line 15\n", "line 15 by B\n");
        std::fs::write(repo.0.join("f.txt"), a_and_b).unwrap();
        repo.run("jj", &["new"]);
        let backend = JjBackend::new(repo.0.clone());
        let a = repo.out(
            "jj",
            &[
                "log",
                "-r",
                "description(exact:\"A\\n\")",
                "--no-graph",
                "-T",
                "change_id",
            ],
        );
        (repo, backend, a)
    }

    #[test]
    fn a_range_lens_stamps_a_line_to_the_change_that_last_touched_it() {
        let (_repo, backend, a) = stack();
        let mut resolver = Resolver::new(&backend);
        let stamp = resolver
            .stamp(&Lens::range(&backend), "f.txt", 10, 10)
            .expect("stamp");
        assert_eq!(stamp.change, a);
        assert_eq!((stamp.start, stamp.end), (10, 10));
        assert_eq!(stamp.snippet, vec!["line 10 by A".to_string()]);
    }

    #[test]
    fn a_stamp_survives_a_rewrite_of_its_change_beneath_the_stack() {
        let (repo, backend, a) = stack();
        let stamp = Resolver::new(&backend)
            .stamp(&Lens::range(&backend), "f.txt", 10, 10)
            .expect("stamp");

        // Rewrite A underneath B: three lines arrive at the top of the file.
        repo.run("jj", &["edit", &a]);
        let text = std::fs::read_to_string(repo.0.join("f.txt")).unwrap();
        std::fs::write(repo.0.join("f.txt"), format!("n1\nn2\nn3\n{text}")).unwrap();
        repo.run("jj", &["new", "heads(all())"]);

        let mut resolver = Resolver::new(&backend);
        match resolver.resolve(&Lens::range(&backend), "f.txt", &stamp) {
            Resolution::At {
                start,
                end,
                stamp: fresh,
            } => {
                assert_eq!((start, end), (13, 13));
                assert_eq!(fresh.change, a);
                assert_ne!(
                    fresh.commit, stamp.commit,
                    "refreshed to the rewritten commit"
                );
                assert_eq!((fresh.start, fresh.end), (13, 13));
            }
            other => panic!("expected the anchor to follow the rewrite, got {other:?}"),
        }
    }

    #[test]
    fn a_stamp_goes_outdated_when_a_later_change_rewrites_its_line() {
        let (repo, backend, _a) = stack();
        let stamp = Resolver::new(&backend)
            .stamp(&Lens::range(&backend), "f.txt", 10, 10)
            .expect("stamp");
        let text = std::fs::read_to_string(repo.0.join("f.txt")).unwrap();
        std::fs::write(
            repo.0.join("f.txt"),
            text.replace("line 10 by A", "line 10 reworded"),
        )
        .unwrap();

        let mut resolver = Resolver::new(&backend);
        assert_eq!(
            resolver.resolve(&Lens::range(&backend), "f.txt", &stamp),
            Resolution::Outdated
        );
    }

    #[test]
    fn a_narrowed_lens_shows_only_its_own_changes_anchors() {
        let (repo, backend, a) = stack();
        let stamp = Resolver::new(&backend)
            .stamp(&Lens::range(&backend), "f.txt", 10, 10)
            .expect("stamp");
        let b = repo.out("jj", &["log", "-r", "@-", "--no-graph", "-T", "commit_id"]);
        let a_commit = repo.out("jj", &["log", "-r", &a, "--no-graph", "-T", "commit_id"]);

        let mut resolver = Resolver::new(&backend);
        assert_eq!(
            resolver.resolve(&Lens::narrowed(b), "f.txt", &stamp),
            Resolution::Elsewhere
        );
        assert!(matches!(
            resolver.resolve(&Lens::narrowed(a_commit), "f.txt", &stamp),
            Resolution::At {
                start: 10,
                end: 10,
                ..
            }
        ));
    }

    #[test]
    fn a_stamp_whose_change_is_gone_falls_back_to_its_snippet() {
        let (repo, backend, a) = stack();
        let stamp = Resolver::new(&backend)
            .stamp(&Lens::range(&backend), "f.txt", 10, 10)
            .expect("stamp");
        // Squash A into B: A's change disappears, its line now belongs to B.
        repo.run(
            "jj",
            &[
                "squash",
                "--from",
                &a,
                "--into",
                "@-",
                "--use-destination-message",
            ],
        );

        let mut resolver = Resolver::new(&backend);
        assert!(matches!(
            resolver.resolve(&Lens::range(&backend), "f.txt", &stamp),
            Resolution::At {
                start: 10,
                end: 10,
                ..
            }
        ));
    }
}
