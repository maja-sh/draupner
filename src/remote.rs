//! Helpers for running commands on the remote server that hosts a workspace's
//! stack. `ws exec`/`up`/`down`/`rm` lean on these to build the `ssh` invocation
//! that `cd`s into a synced worktree and runs there.
//!
//! The SSH binary is resolved through `WS_SSH` (default `ssh`) so tests can
//! substitute a fake that records its arguments instead of dialing a server.
//!
//! [`Site`] is the location-transparency spine: it runs a command or writes a
//! file either locally or on the server, so the stack subsystems (`caddy`,
//! `compose`, `ports`) are written once and work in both places.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// Where a workspace's stack runs, and thus where `ws` drives podman, Caddy,
/// port allocation, and lifecycle scripts. `Local` by default; a remote-offload
/// workspace (a `remote` block in `.ws.json`) drives its Linux server over SSH.
///
/// Both variants honour one contract — `command`/`write_file` produce the same
/// effect here or on the server — so a subsystem takes a `&Site` and stays
/// location-transparent instead of branching on local-vs-remote itself.
pub enum Site {
    Local,
    /// Drive `connection` over SSH. `home` is the server login `$HOME`, resolved
    /// once at construction so server-side paths (Caddy's config dir, the compose
    /// override) are absolute and quote-safe rather than relying on the remote
    /// shell to expand `~`.
    Remote { connection: String, home: String },
}

impl Site {
    /// Resolve the execution site for `cfg`: `Local` with no `remote` block, else
    /// `Remote` after fetching the server's `$HOME`. That fetch doubles as an
    /// early reachability check — a dead or unauthenticated server fails here with
    /// a clear message instead of part-way through bring-up.
    pub fn resolve(cfg: &crate::config::WsConfig) -> Result<Site> {
        match &cfg.remote {
            None => Ok(Site::Local),
            Some(r) => {
                let home = ssh_capture(&r.connection, r#"printf %s "$HOME""#)
                    .map(|h| h.trim().to_string())
                    .filter(|h| !h.is_empty())
                    .ok_or_else(|| {
                        anyhow!(
                            "can't reach the server `{}` over SSH (run `ws remote check`)",
                            r.connection
                        )
                    })?;
                Ok(Site::Remote { connection: r.connection.clone(), home })
            }
        }
    }

    /// Whether the stack runs on a remote server.
    pub fn is_remote(&self) -> bool {
        matches!(self, Site::Remote { .. })
    }

    /// Resolve a server-side absolute path from a `$HOME`-relative one (e.g.
    /// `.config/ws`). Only meaningful for `Remote`; `Local` callers use their own
    /// host paths.
    pub fn home_path(&self, rel: &str) -> Option<String> {
        match self {
            Site::Local => None,
            Site::Remote { home, .. } => Some(format!("{}/{rel}", home.trim_end_matches('/'))),
        }
    }

    /// The workspace's server-side worktree base as an **absolute** path: the
    /// configured `workspace_path` if already absolute, else resolved against the
    /// login `$HOME`. Server compose paths (`COMPOSE_FILE`, the override) must be
    /// absolute — the up script runs with `cd <worktree>`, so a relative
    /// `COMPOSE_FILE` entry would resolve against that cwd and double up. `Local`
    /// returns the base unchanged (host paths need no home resolution).
    pub fn abs_base(&self, base: &str) -> String {
        match self {
            Site::Local => base.to_string(),
            Site::Remote { .. } if base.starts_with('/') => base.to_string(),
            Site::Remote { .. } => self.home_path(base).expect("remote site carries a home"),
        }
    }

    /// A `Command` that runs `argv` at this site — the caller finalizes stdio and
    /// calls `.output()`/`.status()`. Local: `argv[0]` with the rest as args.
    /// Remote: `ssh <conn> '<argv shell-quoted and space-joined>'`, so a single
    /// SSH round trip runs the command server-side.
    pub fn command(&self, argv: &[&str]) -> Command {
        match self {
            Site::Local => {
                let mut c = Command::new(argv[0]);
                c.args(&argv[1..]);
                c
            }
            Site::Remote { connection, .. } => {
                let script: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
                let mut c = ssh_fast(connection);
                c.arg(script.join(" "));
                c
            }
        }
    }

    /// A `Command` that runs a raw shell `script` at this site — for pipelines
    /// and shell builtins where per-arg quoting (see [`command`](Self::command))
    /// would fight the shell. Local: `sh -c <script>`. Remote: `ssh <conn>
    /// <script>` (the script reaches the server shell verbatim, so its own quoting
    /// and `$VAR`s expand there).
    pub fn shell(&self, script: &str) -> Command {
        match self {
            Site::Local => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(script);
                c
            }
            Site::Remote { connection, .. } => {
                let mut c = ssh_fast(connection);
                c.arg(script);
                c
            }
        }
    }

    /// Write `content` to `path` at this site, creating parent dirs. Local:
    /// `fs::write`. Remote: `ssh <conn> 'mkdir -p <dir> && cat > <path>'` with the
    /// content streamed on stdin (so no size/quoting limit on the payload).
    pub fn write_file(&self, path: &str, content: &str) -> Result<()> {
        match self {
            Site::Local => {
                if let Some(parent) = std::path::Path::new(path).parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(path, content).with_context(|| format!("write {path}"))
            }
            Site::Remote { connection, .. } => {
                // Stream the content on stdin (no size/quoting limit on the payload)
                // into a server-side `cat`, creating the parent dir first.
                let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
                let script = format!("mkdir -p {} && cat > {}", shell_quote(dir), shell_quote(path));
                let mut child = self
                    .shell(&script)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("ssh {connection} to write {path}"))?;
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(content.as_bytes())
                    .with_context(|| format!("stream {path} to {connection}"))?;
                let out = child.wait_with_output().context("ssh write")?;
                if !out.status.success() {
                    anyhow::bail!(
                        "write {path} on {connection}: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Ok(())
            }
        }
    }

    /// Remove `path` at this site, best-effort (a missing file is fine). Used by
    /// teardown to drop the generated compose override.
    pub fn remove_file(&self, path: &str) {
        match self {
            Site::Local => {
                let _ = std::fs::remove_file(path);
            }
            Site::Remote { .. } => {
                let _ = self
                    .command(&["rm", "-f", path])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

/// SSH binary to invoke. Overridable via `WS_SSH` for tests.
fn ssh_bin() -> String {
    std::env::var("WS_SSH").unwrap_or_else(|_| "ssh".to_string())
}

/// Non-interactive connect options shared by the fail-fast probes: never prompt
/// for a password (fail instead), and cap the connect wait so a dead host can't
/// stall the caller.
const SSH_FAST_OPTS: &[&str] = &["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"];

/// An `ssh` command to `connection` with the non-interactive fail-fast options
/// applied — the caller appends the remote command/script and sets stdio. The
/// single owner of the `ssh <fast-opts> <dest>` prefix shared by `Site` and the
/// probes.
fn ssh_fast(connection: &str) -> Command {
    let mut c = Command::new(ssh_bin());
    c.args(SSH_FAST_OPTS).arg(connection);
    c
}

/// Whether an SSH destination is safe to pass as an `ssh` argv element. `ssh`
/// parses any argument beginning with `-` as an **option**, so a `connection`
/// like `-oProxyCommand=…` (from a hostile `.ws.json` carried in a cloned repo)
/// would make `ssh` run a local command before connecting — argument injection
/// (CWE-88, the scp/git CVE-2017-1000117 class). Reject a leading dash (and the
/// empty string). `--` is *not* a reliable guard for `ssh`, so we validate the
/// value instead. The single owner of "is this a safe ssh destination", called
/// where a connection enters from config / the CLI.
pub fn connection_looks_safe(connection: &str) -> bool {
    !connection.is_empty() && !connection.starts_with('-')
}

/// Light reachability probe: can we open a non-interactive SSH session to
/// `connection`? `BatchMode=yes` fails fast instead of hanging on a password
/// prompt; a short `ConnectTimeout` avoids stalling on a dead host. Used by
/// `ws init --remote` to catch a bad connection at config time rather than at
/// first `ws new`. (A passphrase-protected key with no agent will read as
/// unreachable — that's what `--no-verify` is for.)
pub fn reachable(connection: &str) -> bool {
    if !connection_looks_safe(connection) {
        return false; // defense-in-depth: never hand ssh an option-shaped host
    }
    ssh_fast(connection)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a **read-only** `script` on `connection` over SSH and capture stdout.
/// `BatchMode`/`ConnectTimeout` fail fast instead of hanging; `None` when SSH
/// itself can't connect (unreachable / auth), so a caller can tell "server said
/// no" from "couldn't reach the server". Used by the `ws remote check` probes —
/// the contract is that `script` never installs, mutates, or `sudo`s.
pub fn ssh_capture(connection: &str, script: &str) -> Option<String> {
    if !connection_looks_safe(connection) {
        return None; // defense-in-depth: never hand ssh an option-shaped host
    }
    let out = ssh_fast(connection)
        .arg(script)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
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

/// Run a lifecycle command on the server: `cd` into the synced worktree, export
/// the `WS_*` contract plus the injected env, and `sh -c <command>`. Inherits
/// stdio (the server's output streams straight through) and mirrors the exit
/// status. The caller flushes the worktree's mutagen sync first so the server
/// sees the latest files.
///
/// The base `WS_*` here mirrors [`crate::hooks`]'s local contract, but the paths
/// are the server's: `WS_WORKTREE` is the synced dir, `WS_WORKSPACE_ROOT` the
/// synced-worktrees base (there's no `.ws.json` workspace root on the server).
pub fn run_event(
    connection: &str,
    worktree_base: &str,
    task: &str,
    repo: &str,
    event: &str,
    command: &str,
    extra_env: &[(String, String)],
) -> std::io::Result<std::process::ExitStatus> {
    let dir = worktree_path(worktree_base, task, repo);
    let mut env: Vec<(&str, &str)> = vec![
        ("WS_REPO", repo),
        ("WS_BRANCH_SANITIZED", task),
        ("WS_HOOK_NAME", event),
        ("WS_WORKTREE", &dir),
        ("WS_WORKSPACE_ROOT", worktree_base),
    ];
    env.extend(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let cmd = ["sh".to_string(), "-c".to_string(), command.to_string()];
    let script = remote_script(&dir, &env, &cmd);
    ssh_command(connection, &script, false).status()
}

/// Assemble the `ssh` command for `connection` running `script`. Passes `-t`
/// (force a remote pty) when `want_tty` — the caller decides based on whether
/// our stdio is interactive; see `exec`. Interactive work (tinker, prompts,
/// colors) gets a pty; piped/redirected/test invocations don't, keeping output
/// clean.
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
    fn site_local_command_runs_the_program_directly() {
        let c = Site::Local.command(&["podman", "ps", "-a"]);
        assert_eq!(c.get_program().to_string_lossy(), "podman");
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["ps", "-a"]);
    }

    #[test]
    fn site_remote_command_wraps_the_whole_command_in_one_ssh_arg() {
        let site = Site::Remote { connection: "srv".into(), home: "/home/u".into() };
        let c = site.command(&["podman", "run", "--name", "x y"]);
        // WS_SSH is unset in the test process → the program is `ssh`.
        assert!(c.get_program().to_string_lossy().ends_with("ssh"));
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        // The destination, then the shell-quoted command joined into a single arg
        // (so the whole thing runs as one remote command).
        assert!(args.contains(&"srv".to_string()), "args: {args:?}");
        assert!(
            args.iter().any(|a| a == "podman run --name 'x y'"),
            "quoted command not a single arg: {args:?}"
        );
    }

    #[test]
    fn site_home_path_is_remote_only() {
        assert_eq!(Site::Local.home_path(".config/ws"), None);
        let site = Site::Remote { connection: "srv".into(), home: "/home/u/".into() };
        assert_eq!(site.home_path(".config/ws").as_deref(), Some("/home/u/.config/ws"));
    }

    #[test]
    fn abs_base_makes_a_relative_server_base_absolute() {
        let site = Site::Remote { connection: "srv".into(), home: "/home/u".into() };
        // The default worktree base is home-relative → resolved against $HOME so
        // `COMPOSE_FILE` doesn't double-resolve under the `cd`'d worktree.
        assert_eq!(site.abs_base("myapp"), "/home/u/myapp");
        // An already-absolute base passes through untouched.
        assert_eq!(site.abs_base("/srv/ws"), "/srv/ws");
        // Local leaves the base alone (host paths need no home resolution).
        assert_eq!(Site::Local.abs_base("myapp"), "myapp");
    }

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
