#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

pub struct WorkspaceFixture {
    pub root: TempDir,
}

impl WorkspaceFixture {
    pub fn empty() -> Self {
        let root = tempfile::Builder::new()
            .prefix("wsfix-")
            .tempdir()
            .expect("tempdir");
        Self { root }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Create a subdirectory and `git init` an empty repo with one empty commit,
    /// so `git worktree add` / `git branch --show-current` etc. work.
    pub fn add_repo(&self, name: &str) -> PathBuf {
        let repo = self.path().join(name);
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                // Isolate from the developer's ~/.gitconfig (gpgsign,
                // hooksPath, commit templates). Without this, a host with
                // `commit.gpgsign = true` can break `git commit --allow-empty`.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "--allow-empty", "-m", "init", "-q"]);
        repo
    }

    /// Run `ws <args>` from the fixture root. Returns Output for inspection.
    ///
    /// Env: `HOME` and `CLAUDE_SKILLS_DIR` are redirected into the fixture
    /// tempdir so a test that inadvertently exercises `ws claude` never
    /// clobbers the developer's real `~/.claude/skills`. Individual tests can
    /// still override either via `run_with_env`.
    pub fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    pub fn run_with_env(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let bin = env!("CARGO_BIN_EXE_ws");
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(self.path())
            .env_remove("RUST_LOG")
            // Redirect HOME + CLAUDE_SKILLS_DIR into the fixture so any op
            // that resolves paths against them lands inside the tempdir.
            .env("HOME", self.path())
            .env("CLAUDE_SKILLS_DIR", self.path().join("claude-skills"));
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.output().expect("spawn ws")
    }

    pub fn run_ok(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "ws {args:?} failed (exit {:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        out
    }
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
