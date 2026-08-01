//! Per-repo lifecycle execution. Two mechanisms behind one interface:
//!
//! - **Declarative `scripts`** in `.ws.json` (preferred) — one or more shell
//!   commands run via `sh -c` in the worktree, with the `WS_*` env.
//! - **Legacy `.ws-hooks/<repo>/<event>` files** (fallback) — an executable run
//!   with the worktree path as `$1`, cwd = the workspace root.
//!
//! `resolve` picks scripts-first-then-file; `run_action` / `capture_action`
//! dispatch. Streamed output goes through `ui::hook_line` so it lands with the
//! correct gutter/indent. See `spec-output-formatting.md`.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::WsConfig;
use crate::ui;

/// Max bytes buffered per streamed line. A hook that emits an unterminated
/// stream (e.g. `yes | tr -d '\n'`) can't OOM us — the excess is discarded
/// up to the next newline.
const MAX_LINE: usize = 8 * 1024;

/// A resolved lifecycle action for a (repo, event): one shell command (a
/// declarative `scripts` entry, preferred, or a synthesized default) or a legacy
/// `.ws-hooks/` file path (fallback).
pub enum Action {
    /// Path to the hook file, relative to the workspace root.
    File(String),
    /// A shell command run in the worktree: a declared `scripts` entry already
    /// joined and anchored by `config::script_command`, or a `ws`-synthesized
    /// container default (see `config::default_script`).
    Command(String),
}

/// Resolve the action for `event`: declared `scripts` win; else a `.ws-hooks/`
/// file (via `lookup`); else, for a container repo, the compose default (see
/// `config::default_script`). `None` when none apply — the caller treats that as
/// a dim skip.
///
/// A declared script arrives as the string `config::script_command` builds — the
/// one owner of "what shell command does this event run", so a `Many` is joined
/// fail-fast with `&&` and a `.ws-hooks/…` reference is anchored to
/// `$WS_WORKSPACE_ROOT`. Deferring that rule to the dispatch site is what once let
/// `capture_action` join *without* anchoring: with cwd = the worktree, a `status`
/// script naming a `.ws-hooks/` path exited 127, the non-zero exit was swallowed,
/// and `ws list` showed an empty status column while the identical config worked
/// for `up` and on a remote workspace.
pub fn resolve(
    cfg: &WsConfig,
    workspace_root: &Path,
    repo: &str,
    event: &str,
    is_container: bool,
) -> Option<Action> {
    if let Some(cmd) = cfg.script_command(repo, event) {
        return Some(Action::Command(cmd));
    }
    let override_path = cfg.repo_config(repo).and_then(|c| c.hooks.get(event).map(String::as_str));
    if let Some(f) = lookup(workspace_root, repo, event, override_path) {
        return Some(Action::File(f));
    }
    is_container
        .then(|| crate::config::default_script(cfg.runtime(), event))
        .flatten()
        .map(Action::Command)
}

/// Find a legacy hook file for the given repo and event. Returns the relative
/// path if it exists and is executable, or `None` if absent/non-executable.
///
/// Checks the override path first (from `RepoConfig.hooks`), then falls back
/// to the convention path `.ws-hooks/<repo>/<event>`.
fn lookup(
    workspace_root: &Path,
    repo: &str,
    event: &str,
    override_path: Option<&str>,
) -> Option<String> {
    if let Some(ovr) = override_path {
        let full = workspace_root.join(ovr);
        if full.exists() && is_executable(&full) {
            return Some(ovr.to_string());
        }
        return None;
    }
    let conv = format!(".ws-hooks/{}/{}", repo, event);
    let full = workspace_root.join(&conv);
    if full.exists() && is_executable(&full) {
        return Some(conv);
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

/// The `WS_*` context every hook/script receives. Single source of truth for the
/// env contract, applied by streamed and captured runs alike.
/// The base `WS_*` env contract every hook/script receives, as owned pairs so the
/// `Command`-based hook runner AND the detached host-process supervisor
/// (`supervise::up`, which never touches a `Command`) apply the identical set.
/// `WS_HOST_*`/`WS_PORT_*` + declared `env` layer on top.
pub fn base_env(
    workspace_root: &Path,
    worktree: &str,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
) -> Vec<(String, String)> {
    vec![
        ("WS_WORKSPACE_ROOT".to_string(), workspace_root.to_string_lossy().into_owned()),
        ("WS_WORKTREE".to_string(), worktree.to_string()),
        ("WS_REPO".to_string(), repo.to_string()),
        ("WS_BRANCH_SANITIZED".to_string(), branch_sanitized.to_string()),
        ("WS_HOOK_NAME".to_string(), event.to_string()),
    ]
}

/// Apply the whole env contract to `c`: the base `WS_*` set (see [`base_env`]),
/// then the caller-computed injection env (`WS_HOST_*`/`WS_PORT_*` + declared
/// `env` mappings) on top. Shared by script and file commands so both mechanisms
/// see the same routing context — and applied as one call, since a declared `env`
/// key only overrides a base `WS_*` key because `extra_env` is applied last.
fn apply_env(
    c: &mut Command,
    workspace_root: &Path,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    extra_env: &[(String, String)],
) {
    let base =
        base_env(workspace_root, &worktree_path.to_string_lossy(), repo, branch_sanitized, event);
    for (k, v) in base.iter().chain(extra_env) {
        c.env(k, v);
    }
}

/// Build the process for a declarative script command: `sh -c <cmd>` with
/// cwd = the worktree and the `WS_*` env plus `WS_WORKTREE`. The single home for
/// the script env contract. Stdio is left to the caller.
fn script_command(
    workspace_root: &Path,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    cmd: &str,
    extra_env: &[(String, String)],
) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).current_dir(worktree_path);
    apply_env(&mut c, workspace_root, worktree_path, repo, branch_sanitized, event, extra_env);
    c
}

/// Build the process for a legacy hook file: executed directly (honoring its
/// shebang) with the worktree path as `$1`, cwd = the workspace root. `hook_rel`
/// must be relative (callers guard). Stdio is left to the caller.
fn file_command(
    workspace_root: &Path,
    hook_rel: &str,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    extra_env: &[(String, String)],
) -> Command {
    let mut c = Command::new(workspace_root.join(hook_rel));
    c.arg(worktree_path).current_dir(workspace_root);
    apply_env(&mut c, workspace_root, worktree_path, repo, branch_sanitized, event, extra_env);
    c
}

/// Run a resolved streaming action (`up` / `down` / `post-create` / `post-remove`).
/// `extra_env` carries the injection context (`WS_HOST_*`/`WS_PORT_*` + declared
/// `env`) the caller computed for this repo; pass `&[]` when there's none.
pub fn run_action(
    workspace_root: &Path,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    action: &Action,
    extra_env: &[(String, String)],
) -> Result<()> {
    match action {
        Action::File(rel) => {
            run(workspace_root, rel, worktree_path, repo, branch_sanitized, event, extra_env)
        }
        Action::Command(cmd) => run_one(
            workspace_root, worktree_path, repo, branch_sanitized, event, cmd, extra_env,
        ),
    }
}

/// Spawn `cmd` with piped stdio, stream its output under `tag`, and fail on a
/// non-zero exit reported as "`{what}` exited N". The single home for the
/// child-lifecycle plumbing shared by the script and file runners.
fn spawn_streamed(
    mut cmd: Command,
    tag: &str,
    spawn_ctx: impl FnOnce() -> String,
    what: &str,
) -> Result<()> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().with_context(spawn_ctx)?;
    let status = stream_and_wait(child, tag)?;
    if !status.success() {
        bail!("{what} exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Run one already-joined shell command in the worktree via a single `sh -c` with
/// cwd = the worktree and the `WS_*` env, streaming its output like a hook. A
/// `Many` arrives already joined with `&&` — sequential and fail-fast, in one
/// shell so `cd`/env carry across commands (the same shape `capture_action` uses,
/// so the two agree). Shared by a declared `scripts` entry and a synthesized
/// container default.
#[allow(clippy::too_many_arguments)]
fn run_one(
    workspace_root: &Path,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    cmd: &str,
    extra_env: &[(String, String)],
) -> Result<()> {
    let c = script_command(
        workspace_root, worktree_path, repo, branch_sanitized, event, cmd, extra_env,
    );
    spawn_streamed(
        c,
        &format!("{repo}-{event}"),
        || format!("spawn `sh -c` for {event} script"),
        &format!("{event} script"),
    )
}

/// Run a legacy hook file: executed directly (honoring its shebang) with the
/// worktree path as `$1` and cwd = the workspace root.
fn run(
    workspace_root: &Path,
    hook_rel: &str,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    extra_env: &[(String, String)],
) -> Result<()> {
    if Path::new(hook_rel).is_absolute() {
        bail!("hook path must be relative to the workspace root: {hook_rel}");
    }
    let tag = format!("{repo}-{event}");
    let cmd = file_command(
        workspace_root, hook_rel, worktree_path, repo, branch_sanitized, event, extra_env,
    );
    spawn_streamed(
        cmd,
        &tag,
        || format!("spawn {}", workspace_root.join(hook_rel).display()),
        hook_rel,
    )
}

/// Stream a child's stdout+stderr through `ui::hook_line` on helper threads and
/// wait for exit. Shared by file-hook `run`, declarative `run_script`, and the
/// remote lifecycle dispatch (`remote::run_event`) so a hook's output gets the
/// same gutter + spinner-suspend whether it ran locally or over SSH. The child
/// must be spawned with stdout+stderr piped.
pub fn stream_and_wait(mut child: Child, tag: &str) -> Result<ExitStatus> {
    let stdout = child.stdout.take().context("child stdout was not piped")?;
    let stderr = child.stderr.take().context("child stderr was not piped")?;
    let tag_out = tag.to_string();
    let tag_err = tag.to_string();

    let out_h = thread::spawn(move || pipe(stdout, &tag_out, false));
    let err_h = thread::spawn(move || pipe(stderr, &tag_err, true));

    let status = child.wait()?;
    let _ = out_h.join();
    let _ = err_h.join();
    Ok(status)
}

/// Run a resolved action for a read-only `status`-style event and capture its
/// first non-empty stdout line, giving up after `timeout`. Used by `ws list`.
#[allow(clippy::too_many_arguments)]
pub fn capture_action(
    workspace_root: &Path,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    action: &Action,
    timeout: Duration,
    extra_env: &[(String, String)],
) -> Option<String> {
    let mut cmd = match action {
        Action::File(rel) => {
            if Path::new(rel).is_absolute() {
                return None;
            }
            file_command(workspace_root, rel, worktree_path, repo, branch_sanitized, event, extra_env)
        }
        // A status probe is normally one command; a `Many` arrives already joined
        // as a fail-fast `cmd1 && cmd2 …` sequence (nothing silently dropped) and
        // its output is captured. See `resolve` for why that join — and the
        // `.ws-hooks/…` anchoring that comes with it — must not be re-derived here.
        Action::Command(cmd) => script_command(
            workspace_root, worktree_path, repo, branch_sanitized, event, cmd, extra_env,
        ),
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let text = capture_within(cmd.spawn().ok()?, timeout)?;
    ui::first_nonempty_line(&text).map(String::from)
}

/// Drain a child's stdout on a helper thread (so a full pipe buffer can't
/// deadlock the wait loop), enforce `timeout` by killing the child, and return
/// everything it wrote. `None` on non-zero exit or timeout.
///
/// The one copy of the process-timeout loop, shared by the `status` capture above
/// and `remote::probe_login_path` (the login-`PATH` harvest) — each keeps its own
/// reading of the captured text, but a fix to the wait/kill loop lands in both.
pub(crate) fn capture_within(mut child: Child, timeout: Duration) -> Option<String> {
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader.join().unwrap_or_default();
                return status.success().then_some(text);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Read lines from `r`, capped at `MAX_LINE` bytes per line, calling `f` for
/// each. Overlong lines are truncated (tail discarded up to the next newline).
/// Invalid UTF-8 is replaced with U+FFFD via `from_utf8_lossy` — never drops
/// following lines. Split from `pipe` so the tests exercise this loop rather
/// than a copy of it.
fn for_each_line<R: Read>(r: R, mut f: impl FnMut(&str)) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        let n = match reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(_) => break,
        };
        // Cap-hit without a newline: drain the rest of the line.
        if n == MAX_LINE && buf.last() != Some(&b'\n') {
            let mut discard = Vec::new();
            let _ = reader.read_until(b'\n', &mut discard);
        }
        let end = if buf.last() == Some(&b'\n') { buf.len() - 1 } else { buf.len() };
        let line = String::from_utf8_lossy(&buf[..end]);
        f(&line);
    }
}

/// Stream lines from `r` to `ui::hook_line` so hook output lands with the
/// correct gutter/indent.
fn pipe<R: Read>(r: R, tag: &str, stderr: bool) {
    for_each_line(r, |line| ui::hook_line(tag, line, stderr));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Drive the real read loop `pipe()` uses so we can assert what the ui layer
    /// would see, without depending on eprintln capture.
    fn read_lines(input: &[u8]) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for_each_line(Cursor::new(input), |l| lines.push(l.to_string()));
        lines
    }

    #[test]
    fn truncates_oversize_line_and_continues() {
        let mut input = vec![b'a'; 20 * 1024];
        input.push(b'\n');
        input.extend_from_slice(b"after\n");
        let lines = read_lines(&input);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_LINE);
        assert_eq!(lines[1], "after");
    }

    #[test]
    fn survives_invalid_utf8_mid_stream() {
        let input: &[u8] = b"before\n\xffbad\nafter\n";
        let lines = read_lines(input);
        assert_eq!(lines.len(), 3);
        assert!(lines[0] == "before");
        assert!(lines[2] == "after");
    }

    #[test]
    fn a_declared_script_resolves_to_one_anchored_command() {
        // `resolve` hands back the string `config::script_command` built: a `Many`
        // already joined fail-fast, and `.ws-hooks/…` already anchored to
        // `$WS_WORKSPACE_ROOT`. Deferring either step to the dispatch site is how
        // the captured (`status`) path once ended up joining without anchoring.
        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","scripts":{"status":[".ws-hooks/status.sh","echo ok"]}}]}"#,
        )
        .expect("parse config");
        let Some(Action::Command(cmd)) = resolve(&cfg, Path::new("/ws"), "api", "status", false)
        else {
            panic!("a declared script must resolve to a command");
        };
        assert_eq!(cmd, "\"$WS_WORKSPACE_ROOT\"/.ws-hooks/status.sh && echo ok");
    }

    #[test]
    fn resolution_falls_back_to_the_container_default_and_then_to_nothing() {
        let cfg: WsConfig =
            serde_json::from_str(r#"{"repos":[{"name":"api"}]}"#).expect("parse config");
        // No declared script and no hook file: a container repo gets the compose
        // default, everything else resolves to nothing (a dim skip for the caller).
        let Some(Action::Command(cmd)) = resolve(&cfg, Path::new("/ws"), "api", "up", true) else {
            panic!("a container repo must fall back to the compose default");
        };
        assert_eq!(cmd, "podman compose up -d");
        assert!(resolve(&cfg, Path::new("/ws"), "api", "up", false).is_none());
        // No default exists for this event, container or not.
        assert!(resolve(&cfg, Path::new("/ws"), "api", "post-create", true).is_none());
    }

    /// A child with piped stdout, for the capture loop.
    fn sh(script: &str) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh")
    }

    #[test]
    fn capture_within_returns_the_whole_output_and_refuses_a_failed_child() {
        // Everything the child wrote, verbatim — reading a *line* out of it is the
        // caller's rule (`ws list` takes the first non-empty one; the login-PATH
        // probe wants the line after its sentinel), not this loop's.
        assert_eq!(
            capture_within(sh("printf 'a\\nb\\n'"), Duration::from_secs(30)).as_deref(),
            Some("a\nb\n")
        );
        // A non-zero exit is no answer at all, whatever landed on stdout.
        assert_eq!(capture_within(sh("echo out; exit 3"), Duration::from_secs(30)), None);
    }

    #[test]
    fn capture_within_kills_a_child_that_outlives_its_timeout() {
        // The path neither caller could afford to get wrong: a wedged `status`
        // script must not stall `ws list`, and a misbehaving interactive rc must
        // not hang the op that harvested its PATH.
        let started = Instant::now();
        assert_eq!(capture_within(sh("sleep 30"), Duration::from_millis(200)), None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout must return without waiting for the child"
        );
    }
}
