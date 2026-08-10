use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    Revision(String),
    /// Latest common ancestor of `@` and `against`. The right base for "show
    /// me what's on this branch and nothing else" — equivalent to git's
    /// `against...@` three-dot form or jj's `fork_point(against | @)`.
    MergeBase {
        against: Box<Base>,
    },
}

impl Base {
    /// The leaf revision string that anchors this base. Used by the git backend
    /// for `git log <ref>..HEAD`, where merge-base/three-dot semantics already
    /// fall out of `<ref>..HEAD` (commits reachable from HEAD but not the ref).
    fn anchor_ref(&self) -> String {
        match self {
            Base::Revision(r) => r.clone(),
            Base::MergeBase { against } => against.anchor_ref(),
        }
    }
}

/// What slice of history we're looking at. `Range` is the default "PR view"
/// (everything between a base and `@`); `Rev` narrows to a single revision's
/// own diff against its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Range(Base),
    Rev(String),
}

/// A revision in the current range. `id` is the canonical handle we pass back
/// to the backend (jj change-id, git sha); `short_id` is the truncated display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rev {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub is_base: bool,
    pub is_head: bool,
    pub is_in_range: bool,
    /// Bookmarks (jj) or branch/tag decorations (git) pointing at this rev.
    pub refs: Vec<String>,
    /// This rev is `trunk()` / the trunk branch.
    pub is_trunk: bool,
    /// This rev is where the current stack forks off trunk. Distinct from
    /// `is_base`: the fork point is worth pointing at even when you're
    /// currently based somewhere else, since it's the base you most often
    /// want next.
    pub is_fork_point: bool,
    /// Whether this rev is an ancestor of `@`. A base that isn't renders the
    /// other line's commits as reversals, so the panel says so rather than
    /// letting the choice look as ordinary as any other.
    pub is_ancestor: bool,
    /// Parent ids, for laying out the graph. Parents outside the window are
    /// kept here and simply ignored by the layout, which is what makes the
    /// drawing stop at the bottom of the slice rather than run off it.
    pub parents: Vec<String>,
    /// Lane glyphs left of this rev's node, filled in by `crate::graph`.
    pub graph: String,
    /// Lanes continuing to the right of the node.
    pub graph_right: String,
    /// Connector drawn above this rev when lines converge into it, e.g.
    /// `├─╯`. Kept on the rev it describes rather than as its own entry so
    /// `revs` stays a list of real revs and every index is still selectable.
    pub graph_join: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
}

impl FileStatus {
    pub fn glyph(self) -> char {
        match self {
            FileStatus::Modified => 'M',
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub status: FileStatus,
}

pub trait Backend: Send + Sync {
    /// Which VCS this backend speaks: `"jj"` or `"git"`. Reported in the
    /// status payload so a companion session knows the model it's driving.
    fn kind(&self) -> &'static str;
    /// Label for a base in the backend's own vocabulary — the exact string
    /// you could paste into `jj diff --from` or `git diff`. This is what
    /// `--base` is matched against and what the companion status reports.
    fn base_label(&self, base: &Base) -> String;
    /// How the base reads in the header. `base_label` answers "what would I
    /// type", which is the wrong question for a status line: nobody wants to
    /// read `fork_point(trunk() | @)` to learn they're looking at the branch
    /// point. Falls back to the raw label for revsets we have no name for.
    fn base_display(&self, base: &Base) -> String;
    /// `ignore_ws` maps to `-w` (`--ignore-all-space`), matching GitHub's
    /// "ignore whitespace" toggle. Passed to the summary command too, so a
    /// whitespace-only file drops out of the tree and the diff together.
    fn list_changes(&self, scope: &Scope, ignore_ws: bool) -> Result<Vec<FileChange>>;
    fn unified_diff(&self, scope: &Scope, ignore_ws: bool) -> Result<String>;
    fn list_revs(&self, base: &Base) -> Result<Vec<Rev>>;
    fn default_bases(&self) -> Vec<Base>;
    /// Raw bytes of `path` as it exists at `rev`. Used by the renderer to
    /// fetch the post-image of a hunk when the diff isn't against the working
    /// copy — i.e. `Scope::Rev`, where reading disk would land on whatever
    /// `@` happens to be instead of the rev being viewed.
    fn file_content(&self, rev: &str, path: &str) -> Result<String>;
}

/// Walk up from cwd looking for `.jj/` (preferred) then `.git/`.
/// jj wins on colocated repos since it's the source of truth for working-copy state.
pub fn detect_backend() -> Result<Arc<dyn Backend>> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let mut dir: Option<&Path> = Some(&cwd);
    while let Some(d) = dir {
        if d.join(".jj").is_dir() {
            return Ok(Arc::new(JjBackend::new()));
        }
        if d.join(".git").exists() {
            return Ok(Arc::new(GitBackend::new()));
        }
        dir = d.parent();
    }
    Err(anyhow!(
        "not inside a jj or git repository (looked from {})",
        cwd.display()
    ))
}

pub struct JjBackend;

impl JjBackend {
    pub fn new() -> Self {
        Self
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("jj")
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn `jj {}`", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`jj {}` exited with {}: {}",
                args.join(" "),
                output.status,
                stderr.trim()
            ));
        }
        String::from_utf8(output.stdout).context("jj output was not utf-8")
    }
}

impl JjBackend {
    /// Native revset string for a base. `MergeBase` uses jj's own
    /// `fork_point()` rather than the hand-rolled `heads(::@ & ::X)`: the
    /// latter is the classic idiom but can yield more than one head across a
    /// criss-cross merge, and every caller here wants a single commit.
    fn revset(base: &Base) -> String {
        match base {
            Base::Revision(r) => r.clone(),
            Base::MergeBase { against } => format!("fork_point({} | @)", Self::revset(against)),
        }
    }

    /// Resolve a revset to exactly one change id, or empty if it doesn't
    /// resolve. `--limit 1` matters: without it a revset returning two commits
    /// concatenates their ids into a string that matches no rev at all, which
    /// silently drops the marker rather than failing loudly.
    fn resolve_change_id(&self, revset: &str) -> String {
        self.run(&[
            "log",
            "-r",
            revset,
            "--limit",
            "1",
            "--no-graph",
            "-T",
            "change_id ++ \"\\n\"",
        ])
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .unwrap_or_default()
    }
}

impl Backend for JjBackend {
    fn kind(&self) -> &'static str {
        "jj"
    }

    fn base_label(&self, base: &Base) -> String {
        Self::revset(base)
    }

    fn base_display(&self, base: &Base) -> String {
        match base {
            Base::Revision(r) => match r.as_str() {
                "@-" => "parent".into(),
                "@--" => "grandparent".into(),
                "trunk()" => "trunk".into(),
                "root()" => "repo root".into(),
                other => other.into(),
            },
            Base::MergeBase { against } => match against.as_ref() {
                Base::Revision(r) if r == "trunk()" => "branch point".into(),
                other => format!("branch point off {}", self.base_display(other)),
            },
        }
    }

    fn list_changes(&self, scope: &Scope, ignore_ws: bool) -> Result<Vec<FileChange>> {
        let mut args = vec!["diff", "--summary"];
        if ignore_ws {
            args.push("-w");
        }
        let rev;
        match scope {
            Scope::Range(base) => {
                rev = Self::revset(base);
                args.push("--from");
                args.push(&rev);
            }
            Scope::Rev(id) => {
                args.push("-r");
                args.push(id);
            }
        }
        let out = self.run(&args)?;
        Ok(out.lines().filter_map(parse_summary_line).collect())
    }

    fn unified_diff(&self, scope: &Scope, ignore_ws: bool) -> Result<String> {
        let mut args = vec!["diff", "--git"];
        if ignore_ws {
            args.push("-w");
        }
        let rev;
        match scope {
            Scope::Range(base) => {
                rev = Self::revset(base);
                args.push("--from");
                args.push(&rev);
            }
            Scope::Rev(id) => {
                args.push("-r");
                args.push(id);
            }
        }
        self.run(&args)
    }

    fn list_revs(&self, base: &Base) -> Result<Vec<Rev>> {
        let base_revset = Self::revset(base);

        let base_id = self.resolve_change_id(&base_revset);
        let wc_id = self.resolve_change_id("@");
        // Landmarks worth pointing at in the picker. Both are cheap single-rev
        // resolves, and both are answers to "where would I plausibly want to
        // be based instead", which is the question the panel exists to answer.
        let trunk_id = self.resolve_change_id("trunk()");
        let fork_id = self.resolve_change_id("fork_point(trunk() | @)");

        // Get the set of change_ids that fall within the range `base_revset..@`
        let range_output = self
            .run(&[
                "log",
                "-r",
                &format!("{base_revset}..@"),
                "--no-graph",
                "-T",
                "change_id ++ \"\n\"",
            ])
            .unwrap_or_default();
        let range_ids: std::collections::HashSet<String> = range_output
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();

        // Which revs are actually on @'s line. Everything else in the window
        // belongs to a branch that merely shares history with it.
        let ancestors: std::collections::HashSet<String> = self
            .run(&[
                "log",
                "-r",
                "::@",
                "--no-graph",
                "-T",
                "change_id ++ \"\\n\"",
            ])
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();

        // History slice behind @ and the base. The window has to be deep
        // enough to *choose* a base from, not just to show the current range,
        // so it reaches further back than the range usually needs. Trunk is
        // unioned in explicitly: on a long-lived branch it can fall outside a
        // plain ancestor slice, and it's the one landmark that must be here.
        //
        // `--no-graph` with an explicit template is the stable contract: we
        // named the fields and separators, so jj changing how it *renders* a
        // log can't reach us. Topology comes back as parent ids and the graph
        // is drawn from those in `crate::graph`, which is why this asks for
        // data rather than glyphs.
        let revset = format!("::@ | ::{base_revset} | ::trunk()");
        let template = r#"change_id ++ "\t" ++ change_id.short(8) ++ "\t" ++ description.first_line() ++ "\t" ++ current_working_copy ++ "\t" ++ bookmarks.join(",") ++ "\t" ++ parents.map(|p| p.change_id()).join(" ") ++ "\n""#;
        let out = self.run(&[
            "log",
            "-r",
            &revset,
            "--limit",
            "40",
            "--no-graph",
            "-T",
            template,
        ])?;

        let mut revs: Vec<Rev> = out.lines().filter_map(parse_jj_rev_line).collect();

        // Post-process to set relationships
        for r in &mut revs {
            r.is_base = r.id == base_id;
            r.is_in_range = range_ids.contains(&r.id);
            r.is_trunk = !trunk_id.is_empty() && r.id == trunk_id;
            r.is_fork_point = !fork_id.is_empty() && r.id == fork_id;
            r.is_ancestor = ancestors.contains(&r.id);
        }

        Ok(draw_graph(revs, &wc_id))
    }

    fn default_bases(&self) -> Vec<Base> {
        // Branch point leads: "what's on this branch and nothing upstream" is
        // the reading recto is for, and it degrades gracefully — sitting
        // directly on trunk, the fork point *is* `@-`, so the plain
        // working-copy case looks the same as it always did.
        vec![
            Base::MergeBase {
                against: Box::new(Base::Revision("trunk()".into())),
            },
            Base::Revision("@-".into()),
            Base::Revision("trunk()".into()),
            Base::Revision("@--".into()),
            Base::Revision("root()".into()),
        ]
    }

    fn file_content(&self, rev: &str, path: &str) -> Result<String> {
        self.run(&["file", "show", "-r", rev, path])
    }
}

pub struct GitBackend {
    repo_root: PathBuf,
}

impl GitBackend {
    pub fn new() -> Self {
        let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self { repo_root }
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo_root)
            .output()
            .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`git {}` exited with {}: {}",
                args.join(" "),
                output.status,
                stderr.trim()
            ));
        }
        String::from_utf8(output.stdout).context("git output was not utf-8")
    }
}

impl GitBackend {
    /// Git diff arg for a base. `MergeBase` becomes git's three-dot syntax
    /// (`X...HEAD`), which means "diff from merge-base(X, HEAD) to HEAD" —
    /// committed-only, excluding the working tree.
    fn diff_arg(base: &Base) -> String {
        match base {
            Base::Revision(r) => r.clone(),
            Base::MergeBase { against } => format!("{}...HEAD", Self::diff_arg(against)),
        }
    }
}

impl Backend for GitBackend {
    fn kind(&self) -> &'static str {
        "git"
    }

    fn base_label(&self, base: &Base) -> String {
        Self::diff_arg(base)
    }

    fn base_display(&self, base: &Base) -> String {
        match base {
            Base::Revision(r) => match r.as_str() {
                "HEAD" => "working tree".into(),
                "HEAD~1" => "previous commit".into(),
                other => other.into(),
            },
            Base::MergeBase { against } => {
                format!("branch point off {}", self.base_display(against))
            }
        }
    }

    fn list_changes(&self, scope: &Scope, ignore_ws: bool) -> Result<Vec<FileChange>> {
        let arg;
        let cmd = match scope {
            Scope::Range(base) => {
                arg = Self::diff_arg(base);
                let mut c = vec!["diff", "--name-status"];
                if ignore_ws {
                    c.push("-w");
                }
                c.push(&arg);
                c
            }
            Scope::Rev(sha) => {
                let mut c = vec!["show", "--name-status", "--format="];
                if ignore_ws {
                    c.push("-w");
                }
                c.push(sha);
                c
            }
        };
        let out = self.run(&cmd)?;
        Ok(out.lines().filter_map(parse_git_name_status).collect())
    }

    fn unified_diff(&self, scope: &Scope, ignore_ws: bool) -> Result<String> {
        let arg;
        let cmd = match scope {
            Scope::Range(base) => {
                arg = Self::diff_arg(base);
                let mut c = vec!["diff"];
                if ignore_ws {
                    c.push("-w");
                }
                c.push(&arg);
                c
            }
            Scope::Rev(sha) => {
                let mut c = vec!["show", "--format="];
                if ignore_ws {
                    c.push("-w");
                }
                c.push(sha);
                c
            }
        };
        self.run(&cmd)
    }

    fn list_revs(&self, base: &Base) -> Result<Vec<Rev>> {
        let base_ref = base.anchor_ref();

        // Resolve base to its full SHA
        let base_id = self
            .run(&["rev-parse", &base_ref])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // Resolve HEAD to its full SHA
        let head_id = self
            .run(&["rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // Get the set of SHAs in `base..HEAD`
        let range_output = self
            .run(&["log", "--format=%H", &format!("{base_ref}..HEAD")])
            .unwrap_or_default();
        let range_ids: std::collections::HashSet<String> = range_output
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|sha| !sha.is_empty())
            .collect();

        // Whichever of main/master this repo actually uses, so the trunk
        // landmark points somewhere real instead of guessing.
        let trunk_ref = ["main", "master"]
            .into_iter()
            .find(|r| self.run(&["rev-parse", "--verify", "--quiet", r]).is_ok());
        let trunk_id = trunk_ref
            .and_then(|r| self.run(&["rev-parse", r]).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let fork_id = trunk_ref
            .and_then(|r| self.run(&["merge-base", r, "HEAD"]).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // History slice behind HEAD and the base. Deep enough to choose a base
        // from, not just to show the current range.
        let out = self.run(&[
            "log",
            "--format=%H%x09%h%x09%s%x09%D",
            "-n",
            "40",
            "HEAD",
            &base_ref,
        ])?;

        let mut revs: Vec<Rev> = out.lines().filter_map(parse_git_rev_line).collect();

        // Which commits are on HEAD's line. jj is the backend that draws a
        // graph; git stays a flat list, so this is the only thing keeping its
        // panel honest about a base off to one side.
        let ancestors: std::collections::HashSet<String> = self
            .run(&["rev-list", "HEAD"])
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|sha| !sha.is_empty())
            .collect();

        // Post-process to set relationships
        for r in &mut revs {
            r.is_base = r.id == base_id;
            r.is_head = r.id == head_id;
            r.is_in_range = range_ids.contains(&r.id);
            r.is_trunk = !trunk_id.is_empty() && r.id == trunk_id;
            r.is_fork_point = !fork_id.is_empty() && r.id == fork_id;
            r.is_ancestor = ancestors.contains(&r.id);
        }

        Ok(revs)
    }

    fn default_bases(&self) -> Vec<Base> {
        // Branch point leads, matching the jj backend.
        vec![
            Base::MergeBase {
                against: Box::new(Base::Revision("main".into())),
            },
            Base::Revision("HEAD".into()),
            Base::Revision("main".into()),
            Base::Revision("master".into()),
            Base::Revision("HEAD~1".into()),
        ]
    }

    fn file_content(&self, rev: &str, path: &str) -> Result<String> {
        self.run(&["show", &format!("{rev}:{path}")])
    }
}

fn parse_jj_rev_line(line: &str) -> Option<Rev> {
    let mut fields = line.splitn(6, '\t');
    let id = fields.next()?.trim().to_string();
    let short_id = fields.next()?.trim().to_string();
    let summary = fields.next().unwrap_or("").trim().to_string();
    let is_wc = fields.next().is_some_and(|s| s.trim() == "true");
    let refs = parse_refs(fields.next().unwrap_or(""), ',');
    let parents: Vec<String> = fields
        .next()
        .unwrap_or("")
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    if id.is_empty() {
        return None;
    }
    let summary = if summary.is_empty() {
        "(no description set)".to_string()
    } else {
        summary
    };
    Some(Rev {
        id,
        short_id,
        summary,
        is_base: false,
        is_head: is_wc,
        is_in_range: false,
        refs,
        is_trunk: false,
        is_fork_point: false,
        is_ancestor: false,
        parents,
        graph: String::new(),
        graph_right: String::new(),
        graph_join: None,
    })
}

/// Reorder revs children-before-parents and fill in their lane glyphs.
/// Splitting this out of `list_revs` keeps the jj call and the drawing
/// separable, and means the drawing is reachable from a test without a repo.
fn draw_graph(revs: Vec<Rev>, priority: &str) -> Vec<Rev> {
    let nodes: Vec<crate::graph::Node<'_>> = revs
        .iter()
        .map(|r| crate::graph::Node {
            id: &r.id,
            parents: &r.parents,
        })
        .collect();
    let order = crate::graph::topo_order(
        &nodes,
        if priority.is_empty() {
            None
        } else {
            Some(priority)
        },
    );

    let ordered: Vec<crate::graph::Node<'_>> = order
        .iter()
        .map(|&i| crate::graph::Node {
            id: &revs[i].id,
            parents: &revs[i].parents,
        })
        .collect();
    let rows = crate::graph::lay_out(&ordered);

    // `order` indexes the original vec, so walk it rather than sorting in
    // place; the rows come back parallel to the ordered view, not the input.
    let mut by_index: Vec<Option<Rev>> = revs.into_iter().map(Some).collect();
    order
        .into_iter()
        .zip(rows)
        .filter_map(|(i, row)| {
            by_index[i].take().map(|mut rev| {
                rev.graph = row.left;
                rev.graph_right = row.right;
                rev.graph_join = row.join;
                rev
            })
        })
        .collect()
}

/// Split a delimited ref list, dropping empties and the noise git's `%D`
/// carries: `HEAD -> main` is the same ref twice, and remote-tracking copies
/// of a local bookmark say nothing the local one didn't.
fn parse_refs(raw: &str, sep: char) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(sep) {
        let name = part.trim().trim_start_matches("HEAD -> ").trim();
        if name.is_empty() || name == "HEAD" {
            continue;
        }
        let short = name.rsplit_once('/').map_or(name, |(_, s)| s);
        if !out.iter().any(|existing| existing == short) {
            out.push(short.to_string());
        }
    }
    out
}

fn parse_git_rev_line(line: &str) -> Option<Rev> {
    let mut fields = line.splitn(4, '\t');
    let id = fields.next()?.trim().to_string();
    let short_id = fields.next()?.trim().to_string();
    let summary = fields.next().unwrap_or("").trim().to_string();
    let refs = parse_refs(fields.next().unwrap_or(""), ',');
    if id.is_empty() {
        return None;
    }
    Some(Rev {
        id,
        short_id,
        summary,
        is_base: false,
        is_head: false,
        is_in_range: false,
        refs,
        is_trunk: false,
        is_fork_point: false,
        is_ancestor: false,
        parents: Vec::new(),
        graph: String::new(),
        graph_right: String::new(),
        graph_join: None,
    })
}

fn parse_summary_line(line: &str) -> Option<FileChange> {
    let (tag, rest) = line.split_once(' ')?;
    let status = match tag {
        "M" => FileStatus::Modified,
        "A" => FileStatus::Added,
        "D" => FileStatus::Deleted,
        "R" => FileStatus::Renamed,
        "C" => FileStatus::Copied,
        _ => return None,
    };
    Some(FileChange {
        path: rest.to_string(),
        status,
    })
}

/// Parse a single line of `git diff --name-status`. Lines are tab-separated;
/// renames/copies look like `R100\told\tnew` or `C75\told\tnew`. For renames
/// and copies, we keep the new path (the one that exists today).
fn parse_git_name_status(line: &str) -> Option<FileChange> {
    let mut fields = line.split('\t');
    let tag = fields.next()?;
    let first = fields.next()?;
    let (status, path) = match tag.chars().next()? {
        'M' => (FileStatus::Modified, first),
        'A' => (FileStatus::Added, first),
        'D' => (FileStatus::Deleted, first),
        'T' => (FileStatus::Modified, first),
        'R' => (FileStatus::Renamed, fields.next().unwrap_or(first)),
        'C' => (FileStatus::Copied, fields.next().unwrap_or(first)),
        _ => return None,
    };
    Some(FileChange {
        path: path.to_string(),
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_drop_head_and_its_arrow_form() {
        assert_eq!(parse_refs("HEAD -> main, origin/main", ','), vec!["main"]);
        assert_eq!(parse_refs("HEAD", ','), Vec::<String>::new());
    }

    #[test]
    fn refs_shorten_and_dedupe_remote_copies() {
        // origin/main says nothing local main didn't, and a bare list of
        // "main main" in the panel reads as a bug.
        assert_eq!(parse_refs("main, origin/main", ','), vec!["main"]);
        assert_eq!(
            parse_refs("feature, origin/other", ','),
            vec!["feature", "other"]
        );
    }

    #[test]
    fn refs_tolerate_empty_and_padded_input() {
        assert_eq!(parse_refs("", ','), Vec::<String>::new());
        assert_eq!(parse_refs("  ,  ", ','), Vec::<String>::new());
        assert_eq!(parse_refs(" main , feat ", ','), vec!["main", "feat"]);
    }

    #[test]
    fn jj_merge_base_uses_fork_point() {
        // The hand-rolled heads(::@ & ::X) form can return two commits on a
        // criss-cross history, and every caller here wants exactly one.
        let base = Base::MergeBase {
            against: Box::new(Base::Revision("trunk()".into())),
        };
        assert_eq!(JjBackend::revset(&base), "fork_point(trunk() | @)");
    }

    #[test]
    fn jj_base_display_names_the_landmarks() {
        let jj = JjBackend::new();
        let fork = Base::MergeBase {
            against: Box::new(Base::Revision("trunk()".into())),
        };
        assert_eq!(jj.base_display(&fork), "branch point");
        assert_eq!(jj.base_display(&Base::Revision("@-".into())), "parent");
        // Anything we have no name for falls through as itself rather than
        // becoming a lie.
        assert_eq!(jj.base_display(&Base::Revision("abc123".into())), "abc123");
    }
}
