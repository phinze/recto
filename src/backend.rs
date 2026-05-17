use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

#[derive(Debug, Clone)]
pub enum Base {
    Revision(String),
}

impl Base {
    pub fn display(&self) -> &str {
        match self {
            Base::Revision(r) => r,
        }
    }
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

pub trait Backend {
    fn list_changes(&self, base: &Base) -> Result<Vec<FileChange>>;
    fn unified_diff(&self, base: &Base) -> Result<String>;
    fn default_bases(&self) -> Vec<Base>;
}

/// Walk up from cwd looking for `.jj/` (preferred) then `.git/`.
/// jj wins on colocated repos since it's the source of truth for working-copy state.
pub fn detect_backend() -> Result<Box<dyn Backend>> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let mut dir: Option<&Path> = Some(&cwd);
    while let Some(d) = dir {
        if d.join(".jj").is_dir() {
            return Ok(Box::new(JjBackend::new()));
        }
        if d.join(".git").exists() {
            return Ok(Box::new(GitBackend::new()));
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

impl Backend for JjBackend {
    fn list_changes(&self, base: &Base) -> Result<Vec<FileChange>> {
        let out = self.run(&["diff", "--summary", "--from", base.display()])?;
        Ok(out.lines().filter_map(parse_summary_line).collect())
    }

    fn unified_diff(&self, base: &Base) -> Result<String> {
        self.run(&["diff", "--git", "--from", base.display()])
    }

    fn default_bases(&self) -> Vec<Base> {
        vec![
            Base::Revision("@-".into()),
            Base::Revision("trunk()".into()),
            Base::Revision("@--".into()),
            Base::Revision("root()".into()),
        ]
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

impl Backend for GitBackend {
    fn list_changes(&self, base: &Base) -> Result<Vec<FileChange>> {
        let out = self.run(&["diff", "--name-status", base.display()])?;
        Ok(out.lines().filter_map(parse_git_name_status).collect())
    }

    fn unified_diff(&self, base: &Base) -> Result<String> {
        self.run(&["diff", base.display()])
    }

    fn default_bases(&self) -> Vec<Base> {
        vec![
            Base::Revision("HEAD".into()),
            Base::Revision("main".into()),
            Base::Revision("master".into()),
            Base::Revision("HEAD~1".into()),
        ]
    }
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
