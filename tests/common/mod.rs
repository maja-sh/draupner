#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// A throwaway HTTP server that answers every request `200 OK` — stands in for
/// the Caddy admin API so a test `ws up`'s route calls succeed without spinning
/// up a real Caddy container / `ws-net`. Pointing `WS_CADDY_ADMIN` at it keeps
/// `ws` in externally-managed mode (no infra created) while letting `route_up`
/// return success, so the routing path is actually exercised. A test that wants
/// to drive the *failure* path overrides `WS_CADDY_ADMIN` to a dead endpoint.
struct AdminStub {
    url: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl AdminStub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin stub");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).ok();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let handle = std::thread::spawn(move || {
            while !sd.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve_200(stream),
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self { url: format!("http://127.0.0.1:{port}"), shutdown, handle: Some(handle) }
    }
}

impl Drop for AdminStub {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Read one HTTP request off `stream` (headers + any declared body, so the peer
/// isn't reset mid-write) and reply `200 OK` with an empty JSON body.
fn serve_200(mut stream: TcpStream) {
    stream.set_read_timeout(Some(Duration::from_millis(1000))).ok();
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let want = content_length(&buf[..pos]);
                    if buf.len() - (pos + 4) >= want {
                        break;
                    }
                }
            }
            Err(_) => break, // timeout — assume the request is fully sent
        }
    }
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
    let _ = stream.flush();
}

/// The `Content-Length` of a request from its raw header bytes (0 if absent).
fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .to_lowercase()
        .lines()
        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0)
}

/// The isolated git config + identity every git invocation in a fixture runs
/// with — including `ws` itself, which shells out to git. Without it the
/// developer's global/system gitconfig (and their commit identity) leaks into
/// the test repos.
fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
}

/// Write a fixture script and make it runnable — one place that knows a script
/// the tests hand to `ws` (stub, hook) has to be 0755.
fn write_exec(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

pub struct WorkspaceFixture {
    pub root: TempDir,
    admin: AdminStub,
    /// Every `add_hook` so far, as `(repo, event, script path)`. Kept so the
    /// `scripts` entries can be re-applied after any `.ws.json` write — tests add
    /// hooks and write their config in either order, and a raw `write_ws_json`
    /// would otherwise drop the entries added before it.
    hooks: std::cell::RefCell<Vec<(String, String, String)>>,
    port_base: Option<u16>,
}

impl WorkspaceFixture {
    pub fn empty() -> Self {
        let root = tempfile::Builder::new()
            .prefix("wsfix-")
            .tempdir()
            .expect("tempdir");
        Self { root, admin: AdminStub::start(), hooks: Default::default(), port_base: None }
    }

    /// Give this fixture its own port window, so a test that asserts on a
    /// *specific* allocation can't be handed the same port as a concurrent one.
    /// Only tests making that kind of assertion need it — everything else is
    /// indifferent to which port it got. Pick bases at least
    /// `MOVED_WIDTH` (500) apart, and outside the default 20000..40000 window.
    pub fn with_port_base(mut self, base: u16) -> Self {
        self.port_base = Some(base);
        self
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
        self.git(name, &["init", "-q", "-b", "main"]);
        self.git(name, &["commit", "--allow-empty", "-m", "init", "-q"]);
        repo
    }

    /// Run a git command inside `<root>/<repo_rel>` with the isolated git env.
    /// Panics on failure. Lets tests drive git directly to set up scenarios
    /// (commits, upstreams, dirty state).
    pub fn git(&self, repo_rel: &str, args: &[&str]) {
        let out = self.git_out(repo_rel, args);
        assert!(
            out.status.success(),
            "git {args:?} in {repo_rel} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Like `git`, but hands back the output instead of asserting success — for
    /// tests that read git's own view of a repo rather than driving it.
    pub fn git_out(&self, repo_rel: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(self.path().join(repo_rel));
        git_env(&mut cmd);
        cmd.output().expect("git")
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
    /// Any hooks already registered via `add_hook` are re-applied on top, so the
    /// two helpers can be called in either order.
    pub fn write_ws_json(&self, json: &str) {
        std::fs::write(self.path().join(".ws.json"), json).expect("write .ws.json");
        self.apply_hook_scripts();
    }

    /// Executable stub at `<root>/<name>` with the given bash `body`. Use for a
    /// canned exit code, e.g. `stub("ok", "exit 0")` / `stub("bad", "exit 1")`
    /// pointed at `WS_SSH`/`WS_MUTAGEN` to simulate reachable/unreachable.
    pub fn stub(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path().join(name);
        write_exec(&path, &format!("#!/usr/bin/env bash\n{body}\n"));
        path
    }

    /// Executable stub at `<root>/<name>` that appends each argv (one per line)
    /// to `$ARGS_FILE`, then exits 0. Stands in for `ssh`/`mutagen` so a test
    /// can assert the exact command `ws` would run without a real server.
    /// Point `WS_SSH`/`WS_MUTAGEN` at it and set `ARGS_FILE`.
    pub fn arg_recorder(&self, name: &str) -> PathBuf {
        self.stub(name, "printf '%s\\n' \"$@\" >> \"$ARGS_FILE\"")
    }

    /// Declare a lifecycle script for `(repo, event)`: `body` (bash commands; the
    /// shebang is prepended) is written executable to `.ws-hooks/<repo>-<event>.sh`
    /// and the repo's `scripts` block in `.ws.json` is pointed at it.
    ///
    /// A file rather than an inline `scripts` string so a multi-line body needs no
    /// JSON escaping; `.ws-hooks/…` is anchored to `$WS_WORKSPACE_ROOT` by `ws`, so
    /// it resolves from the worktree cwd a script runs in.
    pub fn add_hook(&self, repo: &str, event: &str, body: &str) {
        let dir = self.path().join(".ws-hooks");
        std::fs::create_dir_all(&dir).expect("create .ws-hooks");
        let rel = format!(".ws-hooks/{repo}-{event}.sh");
        write_exec(
            &self.path().join(&rel),
            &format!("#!/usr/bin/env bash\nset -euo pipefail\n{body}\n"),
        );
        self.hooks.borrow_mut().push((repo.to_string(), event.to_string(), rel));
        self.apply_hook_scripts();
    }

    /// Declare `cmd` inline as `(repo, event)`'s `scripts` entry — no script file
    /// in between. Use when the test cares that `ws` is the shell's direct parent
    /// (`$PPID`): a `scripts` entry naming a file forks a shell around it, an
    /// inline one does not.
    pub fn add_script(&self, repo: &str, event: &str, cmd: &str) {
        self.hooks.borrow_mut().push((repo.to_string(), event.to_string(), cmd.to_string()));
        self.apply_hook_scripts();
    }

    /// Merge every registered hook into `.ws.json` as a `scripts` entry, promoting
    /// a bare-name repo (`"api"`) to an object entry as needed.
    fn apply_hook_scripts(&self) {
        let hooks = self.hooks.borrow();
        if hooks.is_empty() {
            return;
        }
        let path = self.path().join(".ws.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{\"repos\":[]}".to_string());
        let mut cfg: serde_json::Value = serde_json::from_str(&raw).expect("parse .ws.json");
        let repos = cfg["repos"].as_array_mut().expect("repos array in .ws.json");
        for (repo, event, script) in hooks.iter() {
            let entry = match repos.iter_mut().position(|r| {
                r.as_str() == Some(repo.as_str()) || r["name"].as_str() == Some(repo.as_str())
            }) {
                Some(i) => {
                    if repos[i].is_string() {
                        repos[i] = serde_json::json!({ "name": repo });
                    }
                    &mut repos[i]
                }
                None => {
                    repos.push(serde_json::json!({ "name": repo }));
                    repos.last_mut().unwrap()
                }
            };
            if !entry["scripts"].is_object() {
                entry["scripts"] = serde_json::json!({});
            }
            entry["scripts"][event] = serde_json::Value::String(script.clone());
        }
        std::fs::write(&path, serde_json::to_string_pretty(&cfg).expect("serialize .ws.json"))
            .expect("write .ws.json");
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
            // The HOME redirect below is what isolates `~/.config/ws` (the
            // global config, persisted https_port) into the fixture — an
            // XDG_CONFIG_HOME inherited from the dev shell would bypass it and
            // write into the developer's real config dir.
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", self.path())
            .env("CLAUDE_SKILLS_DIR", self.path().join("claude-skills"))
            // Treat Caddy as externally managed (the always-200 stub) so a test
            // `ws up` never spins up a real Caddy container / `ws-net` yet its
            // route calls still succeed. A test can override this (e.g. to a dead
            // endpoint) to drive the routing-failure path.
            .env("WS_CADDY_ADMIN", self.admin.url.as_str());
        git_env(&mut cmd);
        if let Some(base) = self.port_base {
            cmd.env("WS_PORT_BASE", base.to_string());
        }
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

/// The digits in `<marker>NNNNN]` — the convention a test's `up` script uses to
/// hand an allocated port back through ws's own output.
pub fn port_after(haystack: &str, marker: &str) -> u16 {
    let start = haystack.find(marker).unwrap_or_else(|| panic!("{marker} in output")) + marker.len();
    let end = start + haystack[start..].find(']').expect("closing ]");
    haystack[start..end].parse().unwrap_or(0)
}
