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

    /// Create a workspace with the given repos and a `.ws.json` referencing them.
    pub fn with_repos(names: &[&str]) -> Self {
        let f = Self::empty();
        for name in names {
            f.add_repo(name);
        }
        let repos_json: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
        f.write_ws_json(&format!("{{\"repos\":[{}]}}", repos_json.join(",")));
        f
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

    /// Run a git command inside `<root>/<repo_rel>` with the same isolated env
    /// as `add_repo`. Panics on failure. Lets tests drive git directly to set
    /// up scenarios (commits, upstreams, dirty state).
    pub fn git(&self, repo_rel: &str, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path().join(repo_rel))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} in {repo_rel} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Add a repo with a real bare `origin` remote (under `.origins/`) and its
    /// `main` branch pushed + tracked. Enables upstream-aware checks (unpushed
    /// commits) in tests. Returns the working checkout path.
    pub fn add_repo_with_origin(&self, name: &str) -> PathBuf {
        let repo = self.add_repo(name);
        let origin = self.path().join(".origins").join(format!("{name}.git"));
        std::fs::create_dir_all(origin.parent().unwrap()).expect("mkdir .origins");
        self.git(".", &["init", "--bare", "-q", origin.to_str().unwrap()]);
        self.git(name, &["remote", "add", "origin", origin.to_str().unwrap()]);
        self.git(name, &["push", "-q", "-u", "origin", "main"]);
        repo
    }

    /// Write `.ws.json` in the fixture root with the given raw JSON content.
    pub fn write_ws_json(&self, json: &str) {
        std::fs::write(self.path().join(".ws.json"), json).expect("write .ws.json");
    }

    /// Executable stub at `<root>/<name>` with the given bash `body`. Use for a
    /// canned exit code, e.g. `stub("ok", "exit 0")` / `stub("bad", "exit 1")`
    /// pointed at `WS_SSH`/`WS_MUTAGEN` to simulate reachable/unreachable.
    pub fn stub(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path().join(name);
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n")).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Executable stub at `<root>/<name>` that appends each argv (one per line)
    /// to `$ARGS_FILE`, then exits 0. Stands in for `ssh`/`mutagen` so a test
    /// can assert the exact command `ws` would run without a real server.
    /// Point `WS_SSH`/`WS_MUTAGEN` at it and set `ARGS_FILE`.
    pub fn arg_recorder(&self, name: &str) -> PathBuf {
        let path = self.path().join(name);
        std::fs::write(
            &path,
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> \"$ARGS_FILE\"\n",
        )
        .expect("write recorder");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Write a hook script under `.ws-hooks/<repo>/<event>` and make it executable.
    /// `body` is the script body (bash commands); the shebang is prepended.
    pub fn add_hook(&self, repo: &str, event: &str, body: &str) {
        let dir = self.path().join(".ws-hooks").join(repo);
        std::fs::create_dir_all(&dir).expect("create hook dir");
        let path = dir.join(event);
        std::fs::write(
            &path,
            format!("#!/usr/bin/env bash\nset -euo pipefail\n{body}\n"),
        )
        .expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod hook");
        }
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

    /// Like `run`, but with the process CWD set to `<root>/<subdir>` — for ops
    /// (e.g. `ws here`) whose behavior depends on which worktree you're inside.
    pub fn run_in(&self, subdir: &str, args: &[&str]) -> Output {
        self.run_in_env(subdir, args, &[])
    }

    pub fn run_in_env(&self, subdir: &str, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        self.run_from(&self.path().join(subdir), args, extra_env)
    }

    pub fn run_with_env(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        self.run_from(self.path(), args, extra_env)
    }

    fn run_from(&self, cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let bin = env!("CARGO_BIN_EXE_ws");
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(cwd)
            .env_remove("RUST_LOG")
            .env("HOME", self.path())
            .env("CLAUDE_SKILLS_DIR", self.path().join("claude-skills"))
            // Treat Caddy as externally managed (dead endpoint) so a test `ws up`
            // never spins up a real Caddy container / `ws-net`; route calls fail
            // fast and non-fatally. A routing test can override this.
            .env("WS_CADDY_ADMIN", "http://127.0.0.1:1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
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
