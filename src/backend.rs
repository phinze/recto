use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    Revision(String),
    /// Latest common ancestor of `@` and `against`. The right base for "show
    /// me what's on this branch and nothing else" — equivalent to git's
    /// `against...@` three-dot form or jj's `heads(::@ & ::against)`.
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
    /// you could paste into `jj diff --from` or `git diff`. This is what the
    /// header shows and what `--base` is matched against.
    fn base_label(&self, base: &Base) -> String;
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
    /// Native revset string for a base. `MergeBase` becomes `heads(::@ & ::X)`
    /// which is jj's idiom for the latest common ancestor of `@` and X.
    fn revset(base: &Base) -> String {
        match base {
            Base::Revision(r) => r.clone(),
            Base::MergeBase { against } => format!("heads(::@ & ::{})", Self::revset(against)),
        }
    }
}

impl Backend for JjBackend {
    fn kind(&self) -> &'static str {
        "jj"
    }

    fn base_label(&self, base: &Base) -> String {
        Self::revset(base)
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

        // Resolve base to its canonical change_id
        let base_id = self
            .run(&["log", "-r", &base_revset, "--no-graph", "-T", "change_id"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

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

        // Fetch a slice of recent history (up to 20 revisions) leading to @ and base
        let revset = format!("::@ | ::{base_revset}");
        let template = r#"change_id ++ "\t" ++ change_id.short(8) ++ "\t" ++ description.first_line() ++ "\t" ++ current_working_copy ++ "\n""#;
        let out = self.run(&[
            "log",
            "-r",
            &revset,
            "--limit",
            "20",
            "--no-graph",
            "-T",
            template,
        ])?;

        let mut revs: Vec<Rev> = out.lines().filter_map(parse_jj_rev_line).collect();

        // Post-process to set relationships
        for r in &mut revs {
            r.is_base = r.id == base_id;
            r.is_in_range = range_ids.contains(&r.id);
        }

        Ok(revs)
    }

    fn default_bases(&self) -> Vec<Base> {
        vec![
            Base::Revision("@-".into()),
            Base::MergeBase {
                against: Box::new(Base::Revision("trunk()".into())),
            },
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

        // Fetch a slice of recent history (up to 20 commits) reachable from HEAD or base_ref
        let out = self.run(&[
            "log",
            "--format=%H%x09%h%x09%s",
            "-n",
            "20",
            "HEAD",
            &base_ref,
        ])?;

        let mut revs: Vec<Rev> = out.lines().filter_map(parse_git_rev_line).collect();

        // Post-process to set relationships
        for r in &mut revs {
            r.is_base = r.id == base_id;
            r.is_head = r.id == head_id;
            r.is_in_range = range_ids.contains(&r.id);
        }

        Ok(revs)
    }

    fn default_bases(&self) -> Vec<Base> {
        vec![
            Base::Revision("HEAD".into()),
            Base::MergeBase {
                against: Box::new(Base::Revision("main".into())),
            },
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
    let mut fields = line.splitn(4, '\t');
    let id = fields.next()?.trim().to_string();
    let short_id = fields.next()?.trim().to_string();
    let summary = fields.next().unwrap_or("").trim().to_string();
    let is_wc = fields.next().is_some_and(|s| s.trim() == "true");
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
    })
}

fn parse_git_rev_line(line: &str) -> Option<Rev> {
    let mut fields = line.splitn(3, '\t');
    let id = fields.next()?.trim().to_string();
    let short_id = fields.next()?.trim().to_string();
    let summary = fields.next().unwrap_or("").trim().to_string();
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
