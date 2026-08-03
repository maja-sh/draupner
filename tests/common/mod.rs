#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Answers every request `200 OK`. Pointing `WS_CADDY_ADMIN` at it keeps `ws` in
/// externally-managed mode — no real Caddy container or `ws-net` is created —
/// while `route_up` still succeeds, so the routing path is genuinely exercised.
/// A test drives the failure path by overriding `WS_CADDY_ADMIN` to a dead port.
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

/// The declared body is drained before replying, or the peer is reset mid-write.
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

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .to_lowercase()
        .lines()
        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0)
}

/// Applied to `ws` too, since it shells out to git: without this the developer's
/// global/system gitconfig and commit identity leak into the test repos.
fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
}

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
    /// `(repo, event, script)` for every `add_hook` so far, replayed after each
    /// `.ws.json` write so a later `write_ws_json` can't drop earlier entries.
    hooks: std::cell::RefCell<Vec<(String, String, String)>>,
}

impl WorkspaceFixture {
    pub fn empty() -> Self {
        let root = tempfile::Builder::new()
            .prefix("wsfix-")
            .tempdir()
            .expect("tempdir");
        Self { root, admin: AdminStub::start(), hooks: Default::default() }
    }

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

    /// The empty commit is what makes `git worktree add` and friends work at all.
    pub fn add_repo(&self, name: &str) -> PathBuf {
        let repo = self.path().join(name);
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        self.git(name, &["init", "-q", "-b", "main"]);
        self.git(name, &["commit", "--allow-empty", "-m", "init", "-q"]);
        repo
    }

    pub fn git(&self, repo_rel: &str, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(self.path().join(repo_rel));
        git_env(&mut cmd);
        let out = cmd.output().expect("git");
        assert!(
            out.status.success(),
            "git {args:?} in {repo_rel} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A real bare `origin` with `main` pushed and tracked, so upstream-aware
    /// checks (unpushed commits, `origin/HEAD`) have something to read.
    pub fn add_repo_with_origin(&self, name: &str) -> PathBuf {
        let repo = self.add_repo(name);
        let origin = self.path().join(".origins").join(format!("{name}.git"));
        std::fs::create_dir_all(origin.parent().unwrap()).expect("mkdir .origins");
        self.git(".", &["init", "--bare", "-q", origin.to_str().unwrap()]);
        self.git(name, &["remote", "add", "origin", origin.to_str().unwrap()]);
        self.git(name, &["push", "-q", "-u", "origin", "main"]);
        repo
    }

    /// Registered hooks are re-applied on top, so this and `add_hook` can be
    /// called in either order.
    pub fn write_ws_json(&self, json: &str) {
        std::fs::write(self.path().join(".ws.json"), json).expect("write .ws.json");
        self.apply_hook_scripts();
    }

    /// Point `WS_SSH`/`WS_MUTAGEN` at one of these to stand in for the real
    /// binary — `stub("ok", "exit 0")` / `stub("bad", "exit 1")` for reachability.
    pub fn stub(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path().join(name);
        write_exec(&path, &format!("#!/usr/bin/env bash\n{body}\n"));
        path
    }

    /// Appends each argv one-per-line to `$ARGS_FILE`, so a test can assert the
    /// exact command `ws` would have run without a real server.
    pub fn arg_recorder(&self, name: &str) -> PathBuf {
        self.stub(name, "printf '%s\\n' \"$@\" >> \"$ARGS_FILE\"")
    }

    /// Declares `(repo, event)` as a `.ws-hooks/` script file rather than an
    /// inline `scripts` string, so a multi-line body needs no JSON escaping.
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

    /// Use instead of `add_hook` when the test cares that `ws` is the shell's
    /// direct parent (`$PPID`): a `scripts` entry naming a file forks a shell
    /// around it, an inline one does not.
    pub fn add_script(&self, repo: &str, event: &str, cmd: &str) {
        self.hooks.borrow_mut().push((repo.to_string(), event.to_string(), cmd.to_string()));
        self.apply_hook_scripts();
    }

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

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    /// For ops whose behavior depends on which worktree you're standing in.
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
            // The HOME redirect is what keeps `~/.config/ws` and `~/.claude/skills`
            // out of the developer's real home; an XDG_CONFIG_HOME inherited from
            // the dev shell would bypass it, hence the removal.
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", self.path())
            .env("CLAUDE_SKILLS_DIR", self.path().join("claude-skills"))
            .env("WS_CADDY_ADMIN", self.admin.url.as_str());
        git_env(&mut cmd);
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

/// The digits in `<marker>NNNNN]` — the shape a test's `up` script echoes an
/// allocated port in so it can be read back off ws's own output.
pub fn port_after(haystack: &str, marker: &str) -> u16 {
    let start = haystack.find(marker).unwrap_or_else(|| panic!("{marker} in output")) + marker.len();
    let end = start + haystack[start..].find(']').expect("closing ]");
    haystack[start..end].parse().unwrap_or(0)
}
