//! Per-repo lifecycle execution: declarative `scripts` in `.ws.json` and legacy
//! `.ws-hooks/<repo>/<event>` files, dispatched via `resolve` / `run_action`.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{Script, WsConfig};
use crate::ui;

/// Per-line buffer cap so a hook emitting an unterminated stream can't OOM us;
/// the excess is discarded up to the next newline.
const MAX_LINE: usize = 8 * 1024;

/// A resolved lifecycle action for a (repo, event).
pub enum Action<'a> {
    Script(&'a Script),
    /// Relative to the workspace root.
    File(String),
    /// Synthesized container default (see `config::default_script`).
    Command(String),
}

/// Precedence: declared `scripts`, then a `.ws-hooks/` file, then a synthesized
/// container default; `None` means the caller skips this repo.
pub fn resolve<'a>(
    cfg: &'a WsConfig,
    workspace_root: &Path,
    repo: &str,
    event: &str,
    is_container: bool,
) -> Option<Action<'a>> {
    let rc = cfg.repo_config(repo);
    if let Some(s) = rc.and_then(|c| c.scripts.get(event)) {
        return Some(Action::Script(s));
    }
    let override_path = rc.and_then(|c| c.hooks.get(event).map(String::as_str));
    if let Some(f) = lookup(workspace_root, repo, event, override_path) {
        return Some(Action::File(f));
    }
    is_container
        .then(|| crate::config::default_script(cfg.runtime(), event))
        .flatten()
        .map(Action::Command)
}

/// Return the relative path if the override (from `RepoConfig.hooks`) or the
/// convention `.ws-hooks/<repo>/<event>` exists and is executable.
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

/// Base `WS_*` env every hook/script receives, returned as owned pairs so both
/// the `Command` runner and the detached supervisor (`supervise::up`, which
/// never touches a `Command`) apply the identical set.
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

fn apply_extra(cmd: &mut Command, extra_env: &[(String, String)]) {
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
}

/// Build `sh -c <cmd>` in the worktree with the `WS_*` env; stdio left to caller.
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
    let base = base_env(workspace_root, &worktree_path.to_string_lossy(), repo, branch_sanitized, event);
    apply_extra(&mut c, &base);
    apply_extra(&mut c, extra_env);
    c
}

/// Build the process for a legacy hook file (worktree path as `$1`, cwd =
/// workspace root); `hook_rel` must be relative — callers guard.
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
    let base = base_env(workspace_root, &worktree_path.to_string_lossy(), repo, branch_sanitized, event);
    apply_extra(&mut c, &base);
    apply_extra(&mut c, extra_env);
    c
}

/// Dispatch a resolved streaming action; pass `&[]` for `extra_env` when the
/// caller has no injection context.
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
        Action::Script(s) => {
            run_script(workspace_root, worktree_path, repo, branch_sanitized, event, s, extra_env)
        }
        Action::File(rel) => {
            run(workspace_root, rel, worktree_path, repo, branch_sanitized, event, extra_env)
        }
        Action::Command(cmd) => run_one(
            workspace_root, worktree_path, repo, branch_sanitized, event, cmd, extra_env,
        ),
    }
}

/// `Many` is joined with `&&` in a single shell so `cd`/env carry across
/// commands (same shape as `capture_action`, so the two agree).
fn run_script(
    workspace_root: &Path,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    script: &Script,
    extra_env: &[(String, String)],
) -> Result<()> {
    // Anchor `.ws-hooks/…` to `$WS_WORKSPACE_ROOT` so the same config path
    // resolves whether the hook runs here or on the server.
    let joined = script
        .commands()
        .iter()
        .map(|c| crate::config::anchor_ws_hooks(c))
        .collect::<Vec<_>>()
        .join(" && ");
    run_one(workspace_root, worktree_path, repo, branch_sanitized, event, &joined, extra_env)
}

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
    let mut c = script_command(
        workspace_root, worktree_path, repo, branch_sanitized, event, cmd, extra_env,
    );
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().with_context(|| format!("spawn `sh -c` for {event} script"))?;
    let status = stream_and_wait(child, &format!("{repo}-{event}"))?;
    if !status.success() {
        bail!("{event} script exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Run a legacy hook file (worktree path as `$1`, cwd = workspace root).
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
    let mut cmd = file_command(
        workspace_root, hook_rel, worktree_path, repo, branch_sanitized, event, extra_env,
    );
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", workspace_root.join(hook_rel).display()))?;

    let status = stream_and_wait(child, &tag)?;
    if !status.success() {
        bail!("{hook_rel} exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Shared by local and remote dispatch so a hook's output gets the same gutter
/// and spinner-suspend either way; child must be spawned with both pipes.
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

/// Run an action and capture its first non-empty stdout line, giving up after
/// `timeout`; used by `ws list`.
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
        // `Many` runs as fail-fast `cmd1 && cmd2 …` so nothing is silently dropped.
        Action::Script(s) => {
            let joined = s.commands().join(" && ");
            script_command(
                workspace_root, worktree_path, repo, branch_sanitized, event, &joined, extra_env,
            )
        }
        Action::File(rel) => {
            if Path::new(rel).is_absolute() {
                return None;
            }
            file_command(workspace_root, rel, worktree_path, repo, branch_sanitized, event, extra_env)
        }
        Action::Command(cmd) => script_command(
            workspace_root, worktree_path, repo, branch_sanitized, event, cmd, extra_env,
        ),
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    capture_child(cmd.spawn().ok()?, timeout)
}

/// Drain stdout on a helper thread so a full pipe can't deadlock the wait loop;
/// `None` on non-zero exit, timeout, or empty output.
fn capture_child(mut child: Child, timeout: Duration) -> Option<String> {
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
                if !status.success() {
                    return None;
                }
                return first_nonempty_line(&text);
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

fn first_nonempty_line(s: &str) -> Option<String> {
    s.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from)
}

/// Stream lines to `ui::hook_line`, capped at `MAX_LINE` bytes; the tail after
/// the cap is discarded to the next newline, and invalid UTF-8 becomes U+FFFD
/// via `from_utf8_lossy` so following lines are never dropped.
fn pipe<R: Read>(r: R, tag: &str, stderr: bool) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        let n = match reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        // Cap-hit without a newline: drain the rest so the next read starts on a fresh line.
        if n == MAX_LINE && buf.last() != Some(&b'\n') {
            let mut discard = Vec::new();
            let _ = reader.read_until(b'\n', &mut discard);
        }
        let end = if buf.last() == Some(&b'\n') { buf.len() - 1 } else { buf.len() };
        let line = String::from_utf8_lossy(&buf[..end]);
        ui::hook_line(tag, &line, stderr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Mirror the read loop in `pipe()` so tests assert what ui would see
    /// without depending on eprintln capture.
    fn read_lines(input: &[u8]) -> Vec<String> {
        let cursor = Cursor::new(input);
        let mut reader = BufReader::new(cursor);
        let mut buf = Vec::with_capacity(1024);
        let mut lines: Vec<String> = Vec::new();
        loop {
            buf.clear();
            let n = match reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if n == MAX_LINE && buf.last() != Some(&b'\n') {
                let mut discard = Vec::new();
                let _ = reader.read_until(b'\n', &mut discard);
            }
            let end = if buf.last() == Some(&b'\n') { buf.len() - 1 } else { buf.len() };
            lines.push(String::from_utf8_lossy(&buf[..end]).into_owned());
        }
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
}
