//! Per-repo hook runner. Streams stdout + stderr through `ui::hook_line`
//! so hook output lands with the correct gutter/indent instead of dumping
//! raw to the terminal. See `spec-output-formatting.md` for the visual language.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::ui;

/// Max bytes buffered per streamed line. A hook that emits an unterminated
/// stream (e.g. `yes | tr -d '\n'`) can't OOM us — the excess is discarded
/// up to the next newline.
const MAX_LINE: usize = 8 * 1024;

/// Find a hook for the given repo and event. Returns the relative path to the
/// hook if it exists and is executable, or `None` if absent/non-executable.
///
/// Checks the override path first (from `RepoConfig.hooks`), then falls back
/// to the convention path `.ws-hooks/<repo>/<event>`.
pub fn lookup(workspace_root: &Path, repo: &str, event: &str, override_path: Option<&str>) -> Option<String> {
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

/// The `WS_*` context every hook receives. Single source of truth for the hook
/// environment contract, applied by both `run` (streamed) and `capture`.
fn ws_env(cmd: &mut Command, workspace_root: &Path, repo: &str, branch_sanitized: &str, event: &str) {
    cmd.env("WS_WORKSPACE_ROOT", workspace_root)
        .env("WS_REPO", repo)
        .env("WS_BRANCH_SANITIZED", branch_sanitized)
        .env("WS_HOOK_NAME", event);
}

pub fn run(
    workspace_root: &Path,
    hook_rel: &str,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
) -> Result<()> {
    if Path::new(hook_rel).is_absolute() {
        bail!("hook path must be relative to the workspace root: {hook_rel}");
    }
    let hook = workspace_root.join(hook_rel);
    let tag = format!("{repo}-{event}");

    let mut cmd = Command::new(&hook);
    cmd.arg(worktree_path)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    ws_env(&mut cmd, workspace_root, repo, branch_sanitized, event);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", hook.display()))?;

    let stdout = child.stdout.take().context("child stdout was not piped")?;
    let stderr = child.stderr.take().context("child stderr was not piped")?;
    let tag_out = tag.clone();
    let tag_err = tag.clone();

    let out_h = thread::spawn(move || pipe(stdout, &tag_out, false));
    let err_h = thread::spawn(move || pipe(stderr, &tag_err, true));

    let status = child.wait()?;
    let _ = out_h.join();
    let _ = err_h.join();

    if !status.success() {
        bail!("{hook_rel} exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Run a hook and capture its first non-empty stdout line, giving up after
/// `timeout`. Used by `ws list` for the read-only `status` hook — output is
/// captured (not streamed) and a wedged hook can't hang the whole listing.
///
/// Returns `None` on spawn failure, non-zero exit, timeout, or empty output.
pub fn capture(
    workspace_root: &Path,
    hook_rel: &str,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    timeout: Duration,
) -> Option<String> {
    if Path::new(hook_rel).is_absolute() {
        return None;
    }
    let hook = workspace_root.join(hook_rel);

    let mut cmd = Command::new(&hook);
    cmd.arg(worktree_path)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    ws_env(&mut cmd, workspace_root, repo, branch_sanitized, event);
    let mut child = cmd.spawn().ok()?;

    // Drain stdout on a helper thread so a hook that fills the pipe buffer
    // can't deadlock against our wait loop.
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

/// Stream lines from `r` to `ui::hook_line`, capped at `MAX_LINE` bytes per
/// line. Overlong lines are truncated (tail discarded up to the next newline).
/// Invalid UTF-8 is replaced with U+FFFD via `from_utf8_lossy` — never drops
/// following lines.
fn pipe<R: Read>(r: R, tag: &str, stderr: bool) {
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
        ui::hook_line(tag, &line, stderr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Mirror the read loop in `pipe()` so we can assert what the ui layer
    /// would see, without depending on eprintln capture.
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
