//! What command a `(repo, event)` runs, and running it **here on this machine**.
//! See ARCHITECTURE.md for where this sits.
//!
//! Ops don't call the runners directly — `lifecycle::Ctx` does, as the local arm
//! of the dispatcher. Its remote arm (`remote::run_event`) reuses `base_env` and
//! `stream_and_wait` from here so both sides keep the same env contract and the
//! same output shape.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::WsConfig;
use crate::ui;

/// Max bytes buffered per streamed line, so a hook emitting an unterminated
/// stream (e.g. `yes | tr -d '\n'`) can't OOM us.
const MAX_LINE: usize = 8 * 1024;

/// `None` when the repo declares nothing and has no default — the caller treats
/// that as a dim skip. Errors only for the migration refusal below.
///
/// The command arrives already joined and anchored by `config::script_command`,
/// the one owner of that rule; re-deriving it at a dispatch site is how the
/// captured (`ws list`) path once ran an unanchored `status` script.
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

/// Owned pairs rather than applied to a `Command`, so the detached host-process
/// supervisor (`supervise::up`, which never builds one) applies the identical set.
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

/// `extra_env` goes on last: that ordering is what lets a declared `env` key
/// override a base `WS_*` one.
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

/// Stdio is left to the caller.
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

/// One `sh -c` for the whole already-`&&`-joined command, so `cd` and env carry
/// across a `Many` — the same shape `capture_command` uses, so the two agree.
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

/// Shared with `remote::run_event` so a hook's output gets the same gutter and
/// spinner-suspend whether it ran locally or over SSH.
pub fn stream_and_wait(mut child: Child, tag: &str) -> Result<ExitStatus> {
    let stdout = child.stdout.take().context("child stdout was not piped")?;
    let stderr = child.stderr.take().context("child stderr was not piped")?;
    let tag_out = tag.to_string();
    let tag_err = tag.to_string();

    let out_h = thread::spawn(move || pipe(stdout, &tag_out));
    let err_h = thread::spawn(move || pipe(stderr, &tag_err));

    let status = child.wait()?;
    let _ = out_h.join();
    let _ = err_h.join();
    Ok(status)
}

/// The read-only `status`-style counterpart to [`run_command`], for `ws list`.
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
    let text = capture_within(cmd.spawn().ok()?, timeout)?;
    ui::first_nonempty_line(&text).map(String::from)
}

/// Stdout is drained on a helper thread: a child that fills the pipe buffer would
/// otherwise deadlock the wait loop. `None` on non-zero exit or timeout.
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

/// Split out of `pipe` so the tests drive this loop rather than a copy of it.
fn for_each_line<R: Read>(r: R, mut f: impl FnMut(&str)) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        let n = match reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
            Ok(0) => break,
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

fn pipe<R: Read>(r: R, tag: &str) {
    for_each_line(r, |line| ui::hook_line(tag, line));
}

/// Migration shim — delete a release after 0.6, together with its tests.
///
/// Until 0.6 an executable `.ws-hooks/<repo>/<event>` was discovered by
/// convention and run as that repo's event. Dropping that silently would turn a
/// working hook into a no-op — or, for a container repo, into the compose default
/// — so a leftover file fails its repo loudly instead. A diagnostic, never a
/// resolution rung: it can't produce a command.
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

    /// Drives the real loop `pipe` uses, without depending on eprintln capture.
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
        // Deferring either the join or the anchoring to the dispatch site is how the
        // captured (`status`) path once ended up joining without anchoring.
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
        let cfg: WsConfig =
            serde_json::from_str(r#"{"repos":[{"name":"api"}]}"#).expect("parse config");
        let cmd = resolve(&cfg, Path::new("/ws"), "api", "up", true)
            .expect("resolve")
            .expect("a container repo must fall back to the compose default");
        assert_eq!(cmd, "podman compose up -d");
        assert!(resolve(&cfg, Path::new("/ws"), "api", "up", false).unwrap().is_none());
        // No default exists for this event, container or not.
        assert!(resolve(&cfg, Path::new("/ws"), "api", "post-create", true).unwrap().is_none());
    }

    /// Delete with the shim it covers.
    #[test]
    fn a_leftover_executable_hook_file_fails_with_the_scripts_entry_to_paste() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = dir.path().join(".ws-hooks").join("api");
        std::fs::create_dir_all(&hooks).expect("mkdir");
        let file = hooks.join("up");
        std::fs::write(&file, "#!/bin/sh\necho hi\n").expect("write hook");

        let cfg: WsConfig =
            serde_json::from_str(r#"{"repos":[{"name":"api"}]}"#).expect("parse config");

        // Not executable: already inert before the removal, so nothing to migrate.
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

        // A declared script wins outright, even with the old file still sitting there.
        let migrated: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","scripts":{"up":".ws-hooks/api/up"}}]}"#,
        )
        .expect("parse config");
        assert_eq!(
            resolve(&migrated, dir.path(), "api", "up", true).expect("resolve").as_deref(),
            Some("\"$WS_WORKSPACE_ROOT\"/.ws-hooks/api/up")
        );
    }

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
        // Verbatim: reading a *line* out of it is each caller's rule, not this loop's.
        assert_eq!(
            capture_within(sh("printf 'a\\nb\\n'"), Duration::from_secs(30)).as_deref(),
            Some("a\nb\n")
        );
        assert_eq!(capture_within(sh("echo out; exit 3"), Duration::from_secs(30)), None);
    }

    #[test]
    fn capture_within_kills_a_child_that_outlives_its_timeout() {
        // A wedged `status` script must not stall `ws list`, nor a misbehaving
        // interactive rc the op that harvested its PATH.
        let started = Instant::now();
        assert_eq!(capture_within(sh("sleep 30"), Duration::from_millis(200)), None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout must return without waiting for the child"
        );
    }
}
