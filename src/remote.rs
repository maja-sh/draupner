//! Helpers for running commands on the remote server that hosts a workspace's
//! stack. `ws exec` (and later `up`/`down`/`rm`) lean on these to build the
//! `ssh` invocation that `cd`s into a synced worktree and runs there.
//!
//! The SSH binary is resolved through `WS_SSH` (default `ssh`) so tests can
//! substitute a fake that records its arguments instead of dialing a server.

use std::process::Command;

/// SSH binary to invoke. Overridable via `WS_SSH` for tests.
fn ssh_bin() -> String {
    std::env::var("WS_SSH").unwrap_or_else(|_| "ssh".to_string())
}

/// Server-side path of a synced worktree: `<workspace_path>/<task>/<repo>`.
///
/// Mirrors the local `.worktrees/<task>/<repo>` layout minus the `.worktrees`
/// prefix — on the server these are plain synced directories, not git
/// worktrees. `workspace_path` may be relative (resolved against the SSH login
/// home) or absolute; either way we join with POSIX `/`.
pub fn worktree_path(workspace_path: &str, task: &str, repo: &str) -> String {
    format!("{}/{task}/{repo}", workspace_path.trim_end_matches('/'))
}

/// POSIX single-quote a shell word. Safe tokens pass through unquoted; anything
/// else is wrapped in single quotes with embedded quotes escaped as `'\''`.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| {
            matches!(b,
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'-' | b'_' | b'/' | b'.' | b':' | b'@' | b'%' | b'+' | b'=' | b',')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Build the remote shell script run over SSH: change into `dir`, export the
/// given `WS_*` env pairs, then `exec` the command so the login shell is
/// replaced and the child's exit code propagates back cleanly.
///
/// `cd` failure short-circuits `&&`, so a missing worktree surfaces as the
/// shell's non-zero exit rather than a silent no-op.
pub fn remote_script(dir: &str, env: &[(&str, &str)], command: &[String]) -> String {
    let mut s = format!("cd {} && ", shell_quote(dir));
    for (k, v) in env {
        s.push_str(&format!("{k}={} ", shell_quote(v)));
    }
    s.push_str("exec ");
    let quoted: Vec<String> = command.iter().map(|a| shell_quote(a)).collect();
    s.push_str(&quoted.join(" "));
    s
}

/// Assemble the `ssh` command for `connection` running `script`. Allocates a
/// TTY (`-t`) only when our own stdin is a terminal, matching how a bare `ssh`
/// decides — interactive work (tinker, prompts, colors) gets a pty; piped or
/// test invocations don't, keeping output deterministic.
pub fn ssh_command(connection: &str, script: &str, want_tty: bool) -> Command {
    let mut c = Command::new(ssh_bin());
    if want_tty {
        c.arg("-t");
    }
    c.arg(connection).arg(script);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_path_joins_posix() {
        assert_eq!(worktree_path("workspace", "feat-foo", "api"), "workspace/feat-foo/api");
        assert_eq!(worktree_path("/srv/ws/", "bug-1", "web"), "/srv/ws/bug-1/web");
    }

    #[test]
    fn shell_quote_passes_safe_tokens() {
        assert_eq!(shell_quote("php"), "php");
        assert_eq!(shell_quote("artisan"), "artisan");
        assert_eq!(shell_quote("a/b-c.d:e"), "a/b-c.d:e");
    }

    #[test]
    fn shell_quote_wraps_unsafe_tokens() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn remote_script_cds_exports_and_execs() {
        let cmd = vec!["php".to_string(), "artisan".to_string(), "migrate".to_string()];
        let s = remote_script("workspace/feat-foo/api", &[("WS_REPO", "api")], &cmd);
        assert_eq!(s, "cd workspace/feat-foo/api && WS_REPO=api exec php artisan migrate");
    }

    #[test]
    fn remote_script_quotes_spaces_in_command() {
        let cmd = vec!["echo".to_string(), "hello world".to_string()];
        let s = remote_script("/w/t/r", &[], &cmd);
        assert_eq!(s, "cd /w/t/r && exec echo 'hello world'");
    }
}
