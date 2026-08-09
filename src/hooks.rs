//! Per-repo lifecycle execution: declarative `scripts` in `.ws.json`, dispatched
//! via `resolve` / `run_command`. Streamed output goes through `ui::hook_line`.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::WsConfig;
use crate::ui;

/// Per-line buffer cap so a hook emitting an unterminated stream can't OOM us;
/// the excess is discarded up to the next newline.
const MAX_LINE: usize = 8 * 1024;

/// The shell command for `event`: declared `scripts` entry, else the container default. `None` → dim skip. `.ws-hooks/…` anchoring and `Many`-join happen in `cfg.script_command` so `capture_command` can't drift from `run_command`.
pub fn resolve(
    cfg: &WsConfig,
    workspace_root: &Path,
    repo: &str,
    event: &str,
    is_container: bool,
) -> Result<Option<String>> {
    if let Some(cmd) = cfg.script_command(repo, event) {
        return Ok(Some(cmd));
    }
    refuse_legacy_hook_file(workspace_root, repo, event)?;
    Ok(is_container.then(|| crate::config::default_script(cfg.runtime(), event)).flatten())
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
    apply_env(&mut c, workspace_root, worktree_path, repo, branch_sanitized, event, extra_env);
    c
}

/// Run one already-joined shell command in the worktree, streaming its output as a hook. Shared by a declared `scripts` entry and the synthesized container default; a `Many` arrives fail-fast-joined so `cd`/env carry across commands.
#[allow(clippy::too_many_arguments)]
pub fn run_command(
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

/// Run a `status`-style command and capture its first non-empty stdout line, giving up after `timeout`; used by `ws list`.
#[allow(clippy::too_many_arguments)]
pub fn capture_command(
    workspace_root: &Path,
    worktree_path: &Path,
    repo: &str,
    branch_sanitized: &str,
    event: &str,
    command: &str,
    timeout: Duration,
    extra_env: &[(String, String)],
) -> Option<String> {
    let mut cmd = script_command(
        workspace_root, worktree_path, repo, branch_sanitized, event, command, extra_env,
    );
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

// Migration shim: DELETE a release after 0.6, together with its test. A leftover
// executable `.ws-hooks/<repo>/<event>` would silently become a no-op — or the
// compose default for a container repo — so it fails loudly instead. This is a
// diagnostic; `resolve` only reaches it where the old mechanism would have won.

/// Fail when an executable `.ws-hooks/<repo>/<event>` is left over from the
/// removed hook-file mechanism, naming the `scripts` entry that replaces it and
/// the two contract changes that come with the move.
/// Raise the migration refusal for every `(repo, event)` an op is about to
/// dispatch, **before** it does anything. The check is purely static — config
/// plus one `stat` — so there is no reason for it to surface halfway through.
///
/// It surfaced through `resolve` inside `ws new`'s per-repo loop, which meant a
/// leftover hook file aborted the op after `git worktree add`, seeding and
/// mutagen session creation had already happened for that repo and any repo
/// before it. Worse, the message asks the user to edit `.ws.json` and re-run,
/// and the re-run then fails on the worktrees the aborted attempt left behind.
///
/// Mirrors `resolve`'s condition exactly: a declared `scripts` entry wins, so a
/// repo that has already migrated is not refused for still owning the old file.
pub fn preflight_legacy_hook_files(
    cfg: &WsConfig,
    workspace_root: &Path,
    repos: &[String],
    events: &[&str],
) -> Result<()> {
    for repo in repos {
        for event in events {
            if cfg.script_command(repo, event).is_none() {
                refuse_legacy_hook_file(workspace_root, repo, event)?;
            }
        }
    }
    Ok(())
}

fn refuse_legacy_hook_file(workspace_root: &Path, repo: &str, event: &str) -> Result<()> {
    let rel = format!(".ws-hooks/{repo}/{event}");
    if !is_executable(&workspace_root.join(&rel)) {
        return Ok(());
    }
    bail!(
        "{rel} is an executable hook file, but hook files discovered by convention were \
         removed in ws 0.6 — declare it instead: add \"scripts\": {{\"{event}\": \"{rel}\"}} \
         to {repo}'s entry in .ws.json. The contract changed with it: the script now runs \
         with cwd = the worktree (not the workspace root), and the worktree path arrives as \
         $WS_WORKTREE (not $1)."
    )
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
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

    #[test]
    fn a_declared_script_resolves_to_one_anchored_command() {
        // The `Many`-join and `.ws-hooks/…` anchoring live in `config::script_command`; deferring either to the dispatch site is what once let `capture_command` join without anchoring.
        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","scripts":{"status":[".ws-hooks/status.sh","echo ok"]}}]}"#,
        )
        .expect("parse config");
        let cmd = resolve(&cfg, Path::new("/ws"), "api", "status", false)
            .expect("resolve")
            .expect("a declared script must resolve to a command");
        assert_eq!(cmd, "\"$WS_WORKSPACE_ROOT\"/.ws-hooks/status.sh && echo ok");
    }

    #[test]
    fn resolution_falls_back_to_the_container_default_and_then_to_nothing() {
        // `runtime` pinned so the assertion doesn't depend on which of docker/podman lives on this box's PATH.
        let cfg: WsConfig =
            serde_json::from_str(r#"{"runtime":"podman","repos":[{"name":"api"}]}"#).expect("parse config");
        let cmd = resolve(&cfg, Path::new("/ws"), "api", "up", true)
            .expect("resolve")
            .expect("a container repo must fall back to the compose default");
        assert_eq!(cmd, "podman compose up -d");
        assert!(resolve(&cfg, Path::new("/ws"), "api", "up", false).unwrap().is_none());
        assert!(resolve(&cfg, Path::new("/ws"), "api", "post-create", true).unwrap().is_none());
    }

    /// The migration shim (delete with it): a leftover executable hook file must
    /// fail loudly rather than silently become a no-op — or, worse for a container
    /// repo, silently become the compose default.
    #[test]
    fn a_leftover_executable_hook_file_fails_with_the_scripts_entry_to_paste() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = dir.path().join(".ws-hooks").join("api");
        std::fs::create_dir_all(&hooks).expect("mkdir");
        let file = hooks.join("up");
        std::fs::write(&file, "#!/bin/sh\necho hi\n").expect("write hook");

        let cfg: WsConfig =
            serde_json::from_str(r#"{"runtime":"podman","repos":[{"name":"api"}]}"#).expect("parse config");

        // Not executable: already inert before removal, so a container repo still gets its default.
        assert_eq!(
            resolve(&cfg, dir.path(), "api", "up", true).expect("resolve").as_deref(),
            Some("podman compose up -d")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let err = resolve(&cfg, dir.path(), "api", "up", true)
            .expect_err("an executable hook file must be refused")
            .to_string();
        assert!(err.contains(r#""scripts": {"up": ".ws-hooks/api/up"}"#), "{err}");
        assert!(err.contains("$WS_WORKTREE"), "the new worktree contract must be named: {err}");
        assert!(err.contains("cwd = the worktree"), "the cwd change must be named: {err}");

        // A declared script wins outright — the shim never fires for a repo that already migrated.
        let migrated: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","scripts":{"up":".ws-hooks/api/up"}}]}"#,
        )
        .expect("parse config");
        assert_eq!(
            resolve(&migrated, dir.path(), "api", "up", true).expect("resolve").as_deref(),
            Some("\"$WS_WORKSPACE_ROOT\"/.ws-hooks/api/up")
        );
    }
}
