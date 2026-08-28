use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempRepos(PathBuf);

impl TempRepos {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "recto-cli-repository-{}-{nonce}",
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
