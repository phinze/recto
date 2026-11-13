use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_REPO: AtomicUsize = AtomicUsize::new(0);

struct TempRepos(PathBuf);

impl TempRepos {
    fn new() -> Self {
        // Tests share a process, and macOS clocks are too coarse to tell two
        // of them apart. The timestamp only guards against a recycled pid.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = NEXT_REPO.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "recto-cli-repository-{}-{nonce}-{id}",
            std::process::id()
        ));
        for name in ["caller", "target"] {
            std::fs::create_dir_all(root.join(name).join(".git"))
                .expect("create temporary repository");
        }
        Self(root)
    }

    fn repo(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempRepos {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ping(cwd: &Path, repository: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_recto"));
    command.current_dir(cwd);
    if let Some(repository) = repository {
        command.arg("-R").arg(repository);
    }
    command.arg("ping").output().expect("run recto ping")
}

#[test]
fn repository_override_routes_client_to_target_workspace() {
    let repos = TempRepos::new();
    let caller = repos.repo("caller");
    let target = repos.repo("target");

    let from_caller = ping(&caller, None);
    let from_target = ping(&target, None);
    let redirected = ping(&caller, Some(&target));

    assert_eq!(redirected.status.code(), Some(2));
    assert_eq!(redirected.stderr, from_target.stderr);
    assert_ne!(redirected.stderr, from_caller.stderr);
}

#[test]
fn state_forget_is_an_offline_idempotent_command() {
    let repos = TempRepos::new();
    let workspace = repos.repo("target");
    let output = Command::new(env!("CARGO_BIN_EXE_recto"))
        .env("XDG_STATE_HOME", repos.0.join("state"))
        .args(["state", "forget", "--workspace-root"])
        .arg(&workspace)
        .output()
        .expect("run recto state forget");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

// A rig that coordinates rather than codes has no repositories, but it still
// keeps a briefing and docs. Only a tour, which quotes code, needs repos.
#[test]
fn a_group_without_repositories_keeps_a_briefing_and_docs() {
    let dir = TempRepos::new();
    let group = dir.0.join("cos");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&group).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_recto"))
            .env("XDG_STATE_HOME", &state)
            .env_remove("RIG_BASEDIR")
            .args(args)
            .arg("--group")
            .arg(&group)
            .output()
            .expect("run recto")
    };
    let brief = run(&["brief", "## Needs you\n\nNothing."]);
    assert_eq!(
        brief.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&brief.stderr)
    );
    let doc = run(&["doc", "plan", "The day's plan."]);
    assert_eq!(
        doc.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&doc.stderr)
    );
    let tour = run(&["tour", "# A tour"]);
    assert_eq!(tour.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&tour.stderr).contains("no repositories to tour across"));
}
