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
    /// Where a branch forked off `revision`. The base for an attached pull
    /// request: GitHub's Files-changed tab renders the same three-dot
    /// comparison, so anchoring on the base branch's moving tip instead would
    /// drag that branch's own commits in, inverted, the moment the PR falls
    /// behind it.
    pub fn branch_point(revision: impl Into<String>) -> Self {
        Base::MergeBase {
            against: Box::new(Base::Revision(revision.into())),
        }
    }

    /// The leaf revision string that anchors this base. Used by the git backend
    /// for `git log <ref>..HEAD`, where merge-base/three-dot semantics already
    /// fall out of `<ref>..HEAD` (commits reachable from HEAD but not the ref).
    pub(crate) fn anchor_ref(&self) -> String {
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

/// A revision in the current range. `id` is the immutable handle we pass back
/// to the backend (jj commit id, git sha); `short_id` is the truncated display.
/// A jj change id can name multiple divergent commits, so it is display data,
/// not a safe revision target.
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
    /// Canonical repository root. Backend commands and every path exposed to
    /// the UI are anchored here, regardless of the directory recto started in.
    fn root(&self) -> &Path;
    /// Which VCS this backend speaks: `"jj"` or `"git"`. Reported in the
    /// status payload so a companion session knows the model it's driving.
    fn kind(&self) -> &'static str;
    /// Exact committed revision beneath the mutable working copy. Git names
    /// this `HEAD`; jj review workspaces put a fresh working-copy commit above
    /// the fetched PR head, so their published revision is `@-`.
    fn workspace_revision(&self) -> Result<String>;
    /// Whether this workspace can actually diff from `base`. An attached PR's
    /// base OID comes from GitHub, so it only names a usable base once the base
    /// branch has been fetched here. Asking about the whole base rather than
    /// the commit underneath it matters: a shallow Git checkout can hold both
    /// endpoints while their common ancestor is still missing.
    fn resolves(&self, base: &Base) -> bool;
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
    /// The revision that names "everything up to now" in a range: jj's `@`
    /// (the working-copy commit) or git's `HEAD`.
    fn head_rev(&self) -> &'static str {
        "@"
    }
    /// The single commit a revision names, or `None` when it doesn't resolve.
    fn commit_id(&self, _rev: &str) -> Option<String> {
        None
    }
    /// The commit a base starts from, resolved the way a diff would.
    fn base_commit(&self, _base: &Base) -> Option<String> {
        None
    }
    /// The trunk tip's commit id.
    fn trunk(&self) -> Option<String> {
        None
    }
    /// Whether `ancestor` is `of` or one of its ancestors.
    fn is_ancestor(&self, _ancestor: &str, _of: &str) -> bool {
        false
    }
    /// The commits in `from..to`, newest first, each with its diffstat.
    fn commits(&self, _from: &str, _to: &str) -> Result<Vec<Commit>> {
        Ok(Vec::new())
    }
}

/// One commit in a range, as a revision list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    /// jj's short change id, or git's short sha.
    pub change: String,
    pub summary: String,
    /// Already on trunk.
    pub landed: bool,
    /// Authored by whoever this repository's config says you are.
    pub mine: bool,
    pub empty: bool,
    pub added: u32,
    pub removed: u32,
    /// How long ago it was committed, in the VCS's own words.
    pub when: String,
}

/// Rows of `id, change, empty, landed, email, added, removed, when, summary`
/// separated by 0x1f. A working copy with nothing in it and no description is
/// where the next change will go, not a commit worth listing.
fn parse_commits(out: &str, me: &str) -> Vec<Commit> {
    out.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.splitn(9, '\x1f').collect();
            let [
                id,
                change,
                empty,
                landed,
                email,
                added,
                removed,
                when,
                summary,
            ] = f[..]
            else {
                return None;
            };
            let empty = empty == "e";
            if empty && summary.trim().is_empty() {
                return None;
            }
            Some(Commit {
                id: id.to_string(),
                change: change.to_string(),
                summary: summary.to_string(),
                landed: landed == "l",
                mine: !me.is_empty() && email.trim().eq_ignore_ascii_case(me),
                empty,
                added: added.parse().unwrap_or(0),
                removed: removed.parse().unwrap_or(0),
                when: when.to_string(),
            })
        })
        .collect()
}

fn user_email(cmd: &str, args: &[&str], root: &Path) -> String {
    Command::new(cmd)
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_lowercase())
        .unwrap_or_default()
}

/// First 12 characters of an object id, the width jj and git use for short ids
/// in their own output. Anything shorter than that is already short.
pub(crate) fn short_oid(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

/// Walk up from `start` looking for the nearest jj or git repository root.
pub fn repository_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".jj").is_dir() || d.join(".git").exists() {
            return Some(d.canonicalize().unwrap_or_else(|_| d.to_path_buf()));
        }
        dir = d.parent();
    }
    None
}

/// Detect from cwd. jj wins on colocated repos since it's the source of truth
/// for working-copy state.
pub fn detect_backend() -> Result<Arc<dyn Backend>> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    detect_backend_from(&cwd).ok_or_else(|| {
        anyhow!(
            "not inside a jj or git repository (looked from {})",
            cwd.display()
        )
    })
}

pub(crate) fn detect_backend_from(start: &Path) -> Option<Arc<dyn Backend>> {
    let root = repository_root(start)?;
    if root.join(".jj").is_dir() {
        Some(Arc::new(JjBackend::new(root)))
    } else {
        Some(Arc::new(GitBackend::new(root)))
    }
}

pub struct JjBackend {
    repo_root: PathBuf,
}

impl JjBackend {
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("jj")
            .args(args)
            .current_dir(&self.repo_root)
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

    /// Resolve a revset to exactly one commit id, or empty if it doesn't
    /// resolve. `--limit 1` matters: without it a revset returning two commits
    /// concatenates their ids into a string that matches no rev at all, which
    /// silently drops the marker rather than failing loudly.
    fn resolve_commit_id(&self, revset: &str) -> String {
        self.run(&[
            "log",
            "-r",
            revset,
            "--limit",
            "1",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ])
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .unwrap_or_default()
    }

    /// A local or tracked bookmark between trunk's fork point and the working
    /// copy marks a named layer beneath the current work. Prefer its branch
    /// point when there is exactly one such nearest ancestor; merges with two
    /// candidates are ambiguous and fall back to the ordinary trunk branch
    /// point.
    fn bookmarked_stack_base(&self) -> Option<Base> {
        let (id, bookmarks) = self.bookmarked_head("@-")?;
        // A bookmark with only empty commits above it is the branch being
        // worked on, not a boundary below it: after pushing a branch and
        // starting a fresh change on top, its own bookmark sits on @-, and
        // taking it as the base would diff the branch against itself.
        let work_above = self
            .run(&[
                "log",
                "-r",
                &format!("({id}..@) ~ empty()"),
                "--no-graph",
                "-T",
                "commit_id ++ \"\\n\"",
            ])
            .ok()?;
        let bookmarks = if work_above.trim().is_empty() {
            self.bookmarked_head(&format!("{id}-"))?.1
        } else {
            bookmarks
        };
        Some(Base::MergeBase {
            against: Box::new(Base::Revision(bookmarks[0].clone())),
        })
    }

    /// The nearest bookmarked commit between the trunk fork point (exclusive)
    /// and `ceiling`, with its bookmark names, when there is exactly one and
    /// it isn't a review rig's own bookmark.
    fn bookmarked_head(&self, ceiling: &str) -> Option<(String, Vec<String>)> {
        let out = self
            .run(&[
                "log",
                "-r",
                &format!("heads((bookmarks() | remote_bookmarks()) & fork_point(trunk() | @)::{ceiling})"),
                "--no-graph",
                "-T",
                "commit_id ++ \"\\t\" ++ local_bookmarks.map(|b| b.name()).join(\"\\t\") ++ \"\\t\" ++ remote_bookmarks.map(|b| b.name() ++ \"@\" ++ b.remote()).join(\"\\t\") ++ \"\\n\"",
            ])
            .ok()?;
        let rows: Vec<&str> = out.lines().filter(|line| !line.is_empty()).collect();
        let [row] = rows.as_slice() else {
            return None;
        };
        let mut fields = row.split('\t');
        let candidate_id = fields.next()?;
        if candidate_id == self.resolve_commit_id("fork_point(trunk() | @)") {
            return None;
        }
        let bookmarks: Vec<String> = fields
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        if bookmarks.is_empty() || bookmarks.iter().any(|name| name.starts_with("rig-review/")) {
            return None;
        }
        Some((candidate_id.to_string(), bookmarks))
    }
}

impl Backend for JjBackend {
    fn root(&self) -> &Path {
        &self.repo_root
    }

    fn kind(&self) -> &'static str {
        "jj"
    }

    fn workspace_revision(&self) -> Result<String> {
        let output = self.run(&[
            "log",
            "-r",
            "@-",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ])?;
        let revisions: Vec<&str> = output.lines().filter(|line| !line.is_empty()).collect();
        match revisions.as_slice() {
            [revision] => Ok((*revision).to_string()),
            _ => Err(anyhow!(
                "`@-` resolved to {} revisions; an attached review needs exactly one published head",
                revisions.len()
            )),
        }
    }

    fn resolves(&self, base: &Base) -> bool {
        !self.resolve_commit_id(&Self::revset(base)).is_empty()
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

        let base_id = self.resolve_commit_id(&base_revset);
        let wc_id = self.resolve_commit_id("@");
        // Landmarks worth pointing at in the picker. Both are cheap single-rev
        // resolves, and both are answers to "where would I plausibly want to
        // be based instead", which is the question the panel exists to answer.
        let trunk_id = self.resolve_commit_id("trunk()");
        let fork_id = self.resolve_commit_id("fork_point(trunk() | @)");

        // Get the set of commit ids that fall within the range `base_revset..@`.
        let range_output = self
            .run(&[
                "log",
                "-r",
                &format!("{base_revset}..@"),
                "--no-graph",
                "-T",
                "commit_id ++ \"\n\"",
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
                "commit_id ++ \"\\n\"",
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
        let template = r#"commit_id ++ "\t" ++ change_id.short(8) ++ "\t" ++ description.first_line() ++ "\t" ++ current_working_copy ++ "\t" ++ bookmarks.join(",") ++ "\t" ++ parents.map(|p| p.commit_id()).join(" ") ++ "\n""#;
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
        // A named ancestor after the trunk fork point is a stacked branch
        // boundary. Lead with it when unambiguous, then retain the trunk branch
        // point as both fallback and an explicit wider choice in the picker.
        let trunk = Base::MergeBase {
            against: Box::new(Base::Revision("trunk()".into())),
        };
        let mut bases = vec![
            self.bookmarked_stack_base()
                .unwrap_or_else(|| trunk.clone()),
        ];
        if bases[0] != trunk {
            bases.push(trunk);
        }
        bases.extend([
            Base::Revision("@-".into()),
            Base::Revision("trunk()".into()),
            Base::Revision("@--".into()),
            Base::Revision("root()".into()),
        ]);
        bases
    }

    fn file_content(&self, rev: &str, path: &str) -> Result<String> {
        self.run(&["file", "show", "-r", rev, path])
    }

    fn commit_id(&self, rev: &str) -> Option<String> {
        Some(self.resolve_commit_id(rev)).filter(|id| !id.is_empty())
    }

    fn base_commit(&self, base: &Base) -> Option<String> {
        self.commit_id(&Self::revset(base))
    }

    fn trunk(&self) -> Option<String> {
        self.commit_id("trunk()")
    }

    fn is_ancestor(&self, ancestor: &str, of: &str) -> bool {
        self.commit_id(&format!("({ancestor}) & ::({of})"))
            .is_some()
    }

    fn commits(&self, from: &str, to: &str) -> Result<Vec<Commit>> {
        // Whoever is working in this workspace authored its working copy.
        // Asking that instead of jj's config also reads a review rig's PR
        // commits as someone else's.
        let me = self
            .run(&["log", "-r", "@", "--no-graph", "-T", "author.email()"])
            .map(|e| e.trim().to_lowercase())
            .unwrap_or_default();
        let out = self.run(&[
            "log",
            "-r",
            &format!("({from})..({to})"),
            "--no-graph",
            "-T",
            "commit_id ++ \"\\x1f\" ++ change_id.short(8) ++ \"\\x1f\" ++ if(empty, \"e\") ++ \"\\x1f\" ++ if(self.contained_in(\"::trunk()\"), \"l\") ++ \"\\x1f\" ++ author.email() ++ \"\\x1f\" ++ self.diff().stat().total_added() ++ \"\\x1f\" ++ self.diff().stat().total_removed() ++ \"\\x1f\" ++ committer.timestamp().ago() ++ \"\\x1f\" ++ description.first_line() ++ \"\\n\"",
        ])?;
        Ok(parse_commits(&out, &me))
    }
}

pub struct GitBackend {
    repo_root: PathBuf,
}

impl GitBackend {
    pub fn new(repo_root: PathBuf) -> Self {
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

    /// Commit to pass to `git diff` for a range. A merge-base label such as
    /// `main...HEAD` names the right history relationship for display, but
    /// passing it directly would stop at HEAD and omit working-tree edits.
    fn range_base(&self, base: &Base) -> Result<String> {
        match base {
            Base::Revision(r) => Ok(r.clone()),
            Base::MergeBase { against } => self
                .run(&["merge-base", &against.anchor_ref(), "HEAD"])
                .map(|s| s.trim().to_string()),
        }
    }

    fn trunk_ref(&self) -> Option<String> {
        ["main", "master"]
            .into_iter()
            .find(|r| self.ref_exists(r))
            .map(str::to_string)
            .or_else(|| {
                self.run(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
    }

    fn ref_exists(&self, rev: &str) -> bool {
        self.run(&["rev-parse", "--verify", "--quiet", rev]).is_ok()
    }
}

impl Backend for GitBackend {
    fn root(&self) -> &Path {
        &self.repo_root
    }

    fn kind(&self) -> &'static str {
        "git"
    }

    fn workspace_revision(&self) -> Result<String> {
        self.run(&["rev-parse", "HEAD"])
            .map(|revision| revision.trim().to_string())
    }

    fn resolves(&self, base: &Base) -> bool {
        // `range_base` is what a load would run, so running it here answers the
        // real question. `^{commit}` on the result so a fetched-but-dangling
        // tag or tree cannot pass for something we could diff from.
        match self.range_base(base) {
            Ok(revision) => self.ref_exists(&format!("{revision}^{{commit}}")),
            Err(_) => false,
        }
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
                arg = self.range_base(base)?;
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
                arg = self.range_base(base)?;
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
        let trunk_ref = self.trunk_ref();
        let trunk_id = trunk_ref
            .as_deref()
            .and_then(|r| self.run(&["rev-parse", r]).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let fork_id = trunk_ref
            .as_deref()
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
        let trunk = self.trunk_ref().unwrap_or_else(|| "HEAD".into());
        let mut bases = vec![
            Base::MergeBase {
                against: Box::new(Base::Revision(trunk.clone())),
            },
            Base::Revision("HEAD".into()),
        ];
        for rev in [trunk.as_str(), "main", "master", "HEAD~1"] {
            if rev != "HEAD"
                && self.ref_exists(rev)
                && !bases.iter().any(|base| base.anchor_ref() == rev)
            {
                bases.push(Base::Revision(rev.into()));
            }
        }
        bases
    }

    fn file_content(&self, rev: &str, path: &str) -> Result<String> {
        self.run(&["show", &format!("{rev}:{path}")])
    }

    fn head_rev(&self) -> &'static str {
        "HEAD"
    }

    fn commit_id(&self, rev: &str) -> Option<String> {
        self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    }

    fn base_commit(&self, base: &Base) -> Option<String> {
        self.commit_id(&self.range_base(base).ok()?)
    }

    fn trunk(&self) -> Option<String> {
        self.commit_id(&self.trunk_ref()?)
    }

    fn is_ancestor(&self, ancestor: &str, of: &str) -> bool {
        self.run(&["merge-base", "--is-ancestor", ancestor, of])
            .is_ok()
    }

    fn commits(&self, from: &str, to: &str) -> Result<Vec<Commit>> {
        let me = user_email("git", &["config", "user.email"], &self.repo_root);
        let range = format!("{from}..{to}");
        let out = self.run(&[
            "log",
            "--format=%x1e%H%x1f%h%x1f%ae%x1f%cr%x1f%s",
            "--numstat",
            &range,
        ])?;
        let unlanded: std::collections::HashSet<String> = match self.trunk_ref() {
            Some(trunk) => self
                .run(&["rev-list", &range, "--not", &trunk])?
                .lines()
                .map(str::to_string)
                .collect(),
            None => out
                .split('\x1e')
                .filter_map(|r| r.split('\x1f').next())
                .map(str::to_string)
                .collect(),
        };
        let mut rows = String::new();
        for record in out.split('\x1e').filter(|r| !r.trim().is_empty()) {
            let (head, stats) = record.split_once('\n').unwrap_or((record, ""));
            let f: Vec<&str> = head.splitn(5, '\x1f').collect();
            let [id, short, email, when, summary] = f[..] else {
                continue;
            };
            let (mut added, mut removed, mut files) = (0u32, 0u32, 0);
            for line in stats.lines().filter(|l| !l.is_empty()) {
                let mut cols = line.split('\t');
                added += cols.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                removed += cols.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                files += 1;
            }
            let fields = [
                id,
                short,
                if files == 0 { "e" } else { "" },
                if unlanded.contains(id) { "" } else { "l" },
                email,
                &added.to_string(),
                &removed.to_string(),
                when,
                summary,
            ]
            .join("\x1f");
            rows.push_str(&fields);
            rows.push('\n');
        }
        Ok(parse_commits(&rows, &me))
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

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(name: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!(
                "recto-backend-{name}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).expect("create temp repository");
            Self(root)
        }

        fn run(&self, program: &str, args: &[&str]) {
            let mut command = Command::new(program);
            if program == "jj" {
                command.args([
                    "--config",
                    "user.name='Recto Test'",
                    "--config",
                    "user.email='recto@example.invalid'",
                ]);
            }
            let output = command
                .args(args)
                .current_dir(&self.0)
                .output()
                .expect("run repository command");
            assert!(
                output.status.success(),
                "{program} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
        let jj = JjBackend::new(PathBuf::from("."));
        let fork = Base::MergeBase {
            against: Box::new(Base::Revision("trunk()".into())),
        };
        assert_eq!(jj.base_display(&fork), "branch point");
        assert_eq!(jj.base_display(&Base::Revision("@-".into())), "parent");
        // Anything we have no name for falls through as itself rather than
        // becoming a lie.
        assert_eq!(jj.base_display(&Base::Revision("abc123".into())), "abc123");
    }

    #[test]
    fn jj_revision_rows_remain_selectable_when_a_change_is_divergent() {
        let repo = TempRepo::new("jj-divergent-revision-row");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);

        let remote = repo.0.join("remote.git");
        repo.run("git", &["init", "--bare", remote.to_str().unwrap()]);
        repo.run(
            "jj",
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
        );
        repo.run("jj", &["git", "push", "--bookmark", "main"]);

        std::fs::write(repo.0.join("layer.txt"), "published layer\n").unwrap();
        repo.run("jj", &["describe", "-m", "published layer"]);
        repo.run("jj", &["bookmark", "create", "layer", "-r", "@"]);
        repo.run("jj", &["git", "push", "--bookmark", "layer"]);

        std::fs::write(repo.0.join("layer.txt"), "rewritten layer\n").unwrap();
        repo.run("jj", &["describe", "-m", "rewritten layer"]);
        repo.run("jj", &["bookmark", "move", "layer", "--to", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("top.txt"), "top layer\n").unwrap();

        let backend = JjBackend::new(repo.0.clone());
        let trunk = Base::MergeBase {
            against: Box::new(Base::Revision("trunk()".into())),
        };
        let rev = backend
            .list_revs(&trunk)
            .expect("load revision rows")
            .into_iter()
            .find(|rev| rev.summary == "rewritten layer")
            .expect("find rewritten layer");

        let changes = backend
            .list_changes(&Scope::Rev(rev.id), false)
            .expect("select one revision despite its divergent change id");
        assert!(changes.iter().any(|change| change.path == "layer.txt"));
    }

    #[test]
    fn jj_default_base_prefers_the_nearest_bookmarked_stack_boundary() {
        let repo = TempRepo::new("jj-stacked-base");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("lower.txt"), "lower layer\n").unwrap();
        repo.run("jj", &["describe", "-m", "lower layer"]);
        repo.run("jj", &["bookmark", "create", "stack-base", "-r", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("top.txt"), "top layer\n").unwrap();

        let backend = JjBackend::new(repo.0.clone());
        let base = backend.default_bases().remove(0);
        assert_eq!(backend.base_label(&base), "fork_point(stack-base | @)");
        let changes = backend
            .list_changes(&Scope::Range(base), false)
            .expect("load only the top stack layer");
        assert_eq!(
            changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            vec!["top.txt"]
        );
    }

    #[test]
    fn jj_default_base_skips_the_branchs_own_bookmark_under_an_empty_change() {
        // A pushed branch with a fresh, empty change on top: its own bookmark
        // is on @-, and the diff should still be the branch, not nothing.
        let repo = TempRepo::new("jj-own-bookmark");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("feature.txt"), "feature\n").unwrap();
        repo.run("jj", &["describe", "-m", "feature"]);
        repo.run("jj", &["bookmark", "create", "feature", "-r", "@"]);
        repo.run("jj", &["new"]);

        let backend = JjBackend::new(repo.0.clone());
        let base = backend.default_bases().remove(0);
        let changes = backend
            .list_changes(&Scope::Range(base), false)
            .expect("load the branch");
        assert_eq!(
            changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            vec!["feature.txt"]
        );
    }

    #[test]
    fn jj_default_base_finds_the_layer_below_a_pushed_top_branch() {
        let repo = TempRepo::new("jj-own-bookmark-stack");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("lower.txt"), "lower\n").unwrap();
        repo.run("jj", &["describe", "-m", "lower"]);
        repo.run("jj", &["bookmark", "create", "lower", "-r", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("top.txt"), "top\n").unwrap();
        repo.run("jj", &["describe", "-m", "top"]);
        repo.run("jj", &["bookmark", "create", "top", "-r", "@"]);
        repo.run("jj", &["new"]);

        let backend = JjBackend::new(repo.0.clone());
        let base = backend.default_bases().remove(0);
        assert_eq!(backend.base_label(&base), "fork_point(lower | @)");
        let changes = backend
            .list_changes(&Scope::Range(base), false)
            .expect("load the top layer");
        assert_eq!(
            changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            vec!["top.txt"]
        );
    }

    #[test]
    fn jj_default_base_uses_the_name_of_an_unsynced_bookmark() {
        let repo = TempRepo::new("jj-unsynced-stack-base");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);

        let remote = repo.0.join("remote.git");
        repo.run("git", &["init", "--bare", remote.to_str().unwrap()]);
        repo.run(
            "jj",
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
        );
        repo.run("jj", &["git", "push", "--bookmark", "main"]);

        std::fs::write(repo.0.join("lower.txt"), "lower layer\n").unwrap();
        repo.run("jj", &["describe", "-m", "lower layer"]);
        repo.run("jj", &["bookmark", "create", "stack-base", "-r", "@"]);
        repo.run("jj", &["git", "push", "--bookmark", "stack-base"]);

        std::fs::write(repo.0.join("middle.txt"), "moved stack base\n").unwrap();
        repo.run("jj", &["describe", "-m", "moved stack base"]);
        repo.run("jj", &["bookmark", "move", "stack-base", "--to", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("top.txt"), "top layer\n").unwrap();

        let backend = JjBackend::new(repo.0.clone());
        let base = backend.default_bases().remove(0);
        assert_eq!(backend.base_label(&base), "fork_point(stack-base | @)");
        let changes = backend
            .list_changes(&Scope::Range(base), false)
            .expect("load the layer above the moved bookmark");
        assert_eq!(
            changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            vec!["top.txt"]
        );
    }

    #[test]
    fn jj_default_base_uses_a_tracked_bookmark_when_its_local_copy_diverged() {
        let repo = TempRepo::new("jj-tracked-stack-base");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);

        let remote = repo.0.join("remote.git");
        repo.run("git", &["init", "--bare", remote.to_str().unwrap()]);
        repo.run(
            "jj",
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
        );
        repo.run("jj", &["git", "push", "--bookmark", "main"]);

        std::fs::write(repo.0.join("lower.txt"), "lower layer\n").unwrap();
        repo.run("jj", &["describe", "-m", "lower layer"]);
        repo.run("jj", &["bookmark", "create", "stack-base", "-r", "@"]);
        repo.run("jj", &["git", "push", "--bookmark", "stack-base"]);

        std::fs::write(repo.0.join("local.txt"), "divergent local work\n").unwrap();
        repo.run("jj", &["describe", "-m", "divergent local stack base"]);
        repo.run("jj", &["bookmark", "move", "stack-base", "--to", "@"]);
        repo.run("jj", &["new", "-r", "stack-base@origin"]);
        std::fs::write(repo.0.join("top.txt"), "top layer\n").unwrap();

        let backend = JjBackend::new(repo.0.clone());
        let base = backend.default_bases().remove(0);
        assert_eq!(
            backend.base_label(&base),
            "fork_point(stack-base@origin | @)"
        );
        let changes = backend
            .list_changes(&Scope::Range(base), false)
            .expect("load only the layer above the tracked bookmark");
        assert_eq!(
            changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            vec!["top.txt"]
        );
    }

    #[test]
    fn jj_default_base_ignores_rig_review_pins() {
        let repo = TempRepo::new("jj-review-pin");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);
        repo.run("jj", &["new"]);
        std::fs::write(repo.0.join("review.txt"), "review head\n").unwrap();
        repo.run("jj", &["describe", "-m", "review head"]);
        repo.run(
            "jj",
            &["bookmark", "create", "rig-review/pr-42-repo", "-r", "@"],
        );
        repo.run("jj", &["new"]);

        let backend = JjBackend::new(repo.0.clone());
        let base = backend.default_bases().remove(0);
        assert_eq!(backend.base_label(&base), "fork_point(trunk() | @)");
    }

    #[test]
    fn jj_backend_reports_root_relative_paths_from_a_nested_start() {
        let repo = TempRepo::new("jj-root");
        repo.run("jj", &["git", "init", "--colocate", "."]);
        let nested = repo.0.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("main.rs"), "fn main() {}\n").unwrap();

        let backend = detect_backend_from(&nested).expect("detect nested jj repo");
        assert_eq!(backend.kind(), "jj");
        assert_eq!(backend.root(), repo.0.canonicalize().unwrap());
        let changes = backend
            .list_changes(&Scope::Range(Base::Revision("root()".into())), false)
            .unwrap();
        assert!(changes.iter().any(|change| change.path == "src/main.rs"));
        assert!(changes.iter().all(|change| !change.path.starts_with("../")));
    }

    #[test]
    fn git_resolves_rejects_a_base_whose_merge_base_is_unreachable() {
        let repo = TempRepo::new("git-unrelated");
        repo.run("git", &["init", "-b", "main"]);
        repo.run("git", &["config", "user.name", "Recto Test"]);
        repo.run("git", &["config", "user.email", "recto@example.invalid"]);
        repo.run("git", &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.0.join("a.txt"), "a\n").unwrap();
        repo.run("git", &["add", "."]);
        repo.run("git", &["commit", "-m", "trunk"]);
        // An unrelated root stands in for a shallow checkout: both endpoints
        // are present, but no common ancestor is.
        repo.run("git", &["checkout", "--orphan", "elsewhere"]);
        std::fs::write(repo.0.join("b.txt"), "b\n").unwrap();
        repo.run("git", &["add", "."]);
        repo.run("git", &["commit", "-m", "unrelated"]);

        let backend = GitBackend::new(repo.0.clone());
        // The commit is right there, so asking only whether it exists would
        // say yes; the base built on it still cannot be loaded.
        assert!(backend.ref_exists("main"));
        assert!(backend.resolves(&Base::Revision("main".into())));
        assert!(!backend.resolves(&Base::branch_point("main")));
    }

    #[test]
    fn git_backend_uses_the_real_trunk_and_includes_working_tree_edits() {
        let repo = TempRepo::new("git-master");
        repo.run("git", &["init", "-b", "master"]);
        repo.run("git", &["config", "user.name", "Recto Test"]);
        repo.run("git", &["config", "user.email", "recto@example.invalid"]);
        repo.run("git", &["config", "commit.gpgsign", "false"]);
        let nested = repo.0.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("main.rs"), "fn main() {}\n").unwrap();
        repo.run("git", &["add", "."]);
        repo.run("git", &["commit", "-m", "initial"]);
        std::fs::write(nested.join("main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();

        let backend = detect_backend_from(&nested).expect("detect nested git repo");
        assert_eq!(backend.kind(), "git");
        assert_eq!(backend.root(), repo.0.canonicalize().unwrap());
        let base = backend.default_bases().remove(0);
        assert_eq!(backend.base_label(&base), "master...HEAD");
        let changes = backend
            .list_changes(&Scope::Range(base), false)
            .expect("load working tree against branch point");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/main.rs");
    }

    /// A jj repo whose own config names the test author, so `commits` can
    /// tell whose work is whose. Starts with a described trunk commit under
    /// `main`, and returns it with that commit's id.
    /// A jj repo with a described trunk commit under `main`, pushed to a bare
    /// remote so `trunk()` follows `main@origin`. Returns the repo, the remote
    /// (kept alive beside it, outside the working copy), and the trunk
    /// commit's id. Nothing here writes jj config: under a build sandbox there
    /// is no home to write it to.
    fn jj_trunk_repo(name: &str) -> (TempRepo, TempRepo, String) {
        let repo = TempRepo::new(name);
        let remote = TempRepo::new(&format!("{name}-remote"));
        repo.run("git", &["init", "--bare", remote.0.to_str().unwrap()]);
        repo.run("jj", &["git", "init", "--colocate", "."]);
        repo.run(
            "jj",
            &["git", "remote", "add", "origin", remote.0.to_str().unwrap()],
        );
        std::fs::write(repo.0.join("trunk.txt"), "trunk\n").unwrap();
        repo.run("jj", &["describe", "-m", "trunk"]);
        repo.run("jj", &["bookmark", "create", "main", "-r", "@"]);
        // Pushing makes the trunk commit immutable, so jj starts a fresh
        // working copy above it on its own.
        repo.run("jj", &["git", "push", "--bookmark", "main"]);
        let start = JjBackend::new(repo.0.clone()).commit_id("main").unwrap();
        (repo, remote, start)
    }

    /// Lands everything below the working copy on trunk.
    fn jj_land(repo: &TempRepo) {
        repo.run("jj", &["bookmark", "set", "main", "-r", "@-"]);
        repo.run("jj", &["git", "push", "--bookmark", "main"]);
    }

    fn jj_commit(repo: &TempRepo, file: &str, lines: &str, message: &str) {
        std::fs::write(repo.0.join(file), lines).unwrap();
        repo.run("jj", &["describe", "-m", message]);
        repo.run("jj", &["new"]);
    }

    #[test]
    fn jj_commits_list_the_range_newest_first_with_stats() {
        let (repo, _remote, start) = jj_trunk_repo("jj-commits");
        jj_commit(&repo, "a.txt", "one\ntwo\n", "first");
        jj_land(&repo);
        jj_commit(&repo, "b.txt", "three\n", "second");

        let backend = JjBackend::new(repo.0.clone());
        let commits = backend.commits(&start, "@").expect("list commits");
        // The empty working copy on top isn't a commit worth listing.
        let got: Vec<_> = commits
            .iter()
            .map(|c| (c.summary.as_str(), c.landed, c.mine, c.added))
            .collect();
        assert_eq!(
            got,
            vec![("second", false, true, 1), ("first", true, true, 2)]
        );
    }

    #[test]
    fn page_anchors_at_the_rig_start_once_its_work_landed() {
        let (repo, _remote, start) = jj_trunk_repo("jj-anchor-landed");
        jj_commit(&repo, "a.txt", "a\n", "pushed one");
        jj_commit(&repo, "b.txt", "b\n", "pushed two");
        jj_land(&repo);

        let backend = JjBackend::new(repo.0.clone());
        let branch = backend.default_bases().remove(0);
        assert!(
            backend
                .list_changes(&Scope::Range(branch.clone()), false)
                .unwrap()
                .is_empty(),
            "pushed work leaves the branch point empty"
        );
        let anchor = crate::serve::anchor(&backend, &branch, Some(&start), true);
        assert_eq!(anchor.base, Base::Revision(start.clone()));
        assert!(anchor.label.unwrap().starts_with("rig start"));
        let files = backend
            .list_changes(&Scope::Range(anchor.base), false)
            .unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn page_keeps_the_branch_point_when_others_landed_work_in_between() {
        let (repo, _remote, start) = jj_trunk_repo("jj-anchor-foreign");
        jj_commit(&repo, "mine.txt", "mine\n", "mine");
        std::fs::write(repo.0.join("theirs.txt"), "theirs\n").unwrap();
        repo.run("jj", &["describe", "-m", "theirs"]);
        repo.run("jj", &["new"]);
        repo.run(
            "jj",
            &[
                "metaedit",
                "--author",
                "Someone Else <someone@example.invalid>",
                "@-",
            ],
        );
        jj_land(&repo);

        let backend = JjBackend::new(repo.0.clone());
        let branch = backend.default_bases().remove(0);
        let anchor = crate::serve::anchor(&backend, &branch, Some(&start), true);
        assert_eq!(anchor.base, branch);
        assert!(anchor.note.unwrap().contains("other people"));
    }

    #[test]
    fn page_keeps_the_branch_point_for_unlanded_work() {
        let (repo, _remote, start) = jj_trunk_repo("jj-anchor-branch");
        jj_commit(&repo, "lower.txt", "lower\n", "lower");
        repo.run("jj", &["bookmark", "create", "lower", "-r", "@-"]);
        jj_commit(&repo, "top.txt", "top\n", "top");

        let backend = JjBackend::new(repo.0.clone());
        let branch = backend.default_bases().remove(0);
        // A stack's lower layer isn't trunk, so the start doesn't pull it in.
        let anchor = crate::serve::anchor(&backend, &branch, Some(&start), false);
        assert_eq!(anchor.base, branch);
        assert!(anchor.note.is_none());
    }

    #[test]
    fn git_commits_mark_what_landed() {
        let repo = TempRepo::new("git-commits");
        repo.run("git", &["init", "-b", "main"]);
        repo.run("git", &["config", "user.name", "Recto Test"]);
        repo.run("git", &["config", "user.email", "recto@example.invalid"]);
        repo.run("git", &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.0.join("a.txt"), "a\n").unwrap();
        repo.run("git", &["add", "."]);
        repo.run("git", &["commit", "-m", "start"]);
        let backend = GitBackend::new(repo.0.clone());
        let start = backend.commit_id("HEAD").unwrap();
        std::fs::write(repo.0.join("b.txt"), "b\nb\n").unwrap();
        repo.run("git", &["add", "."]);
        repo.run("git", &["commit", "-m", "landed"]);
        repo.run("git", &["checkout", "-b", "work"]);
        std::fs::write(repo.0.join("c.txt"), "c\n").unwrap();
        repo.run("git", &["add", "."]);
        repo.run("git", &["commit", "-m", "in flight"]);

        let got: Vec<_> = backend
            .commits(&start, "HEAD")
            .unwrap()
            .into_iter()
            .map(|c| (c.summary, c.landed, c.mine, c.added))
            .collect();
        assert_eq!(
            got,
            vec![
                ("in flight".into(), false, true, 1),
                ("landed".into(), true, true, 2)
            ]
        );
    }
}
