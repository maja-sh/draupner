//! SSH plumbing and the [`Site`] abstraction that lets stack subsystems run
//! either locally or on a remote server without branching on which. The SSH
//! binary is resolved through `WS_SSH` so tests can substitute a fake.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::process::{Command, Stdio};

/// Where a workspace's stack runs; one contract, so subsystems don't branch on
/// local-vs-remote.
pub enum Site {
    Local,
    /// `home` is captured once so server paths stay absolute without relying on
    /// remote-shell `~` expansion.
    Remote { connection: String, home: String },
}

impl Site {
    /// The `$HOME` fetch doubles as reachability check: fail here, not part-way through bring-up.
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

    pub fn is_remote(&self) -> bool {
        matches!(self, Site::Remote { .. })
    }

    pub fn home_path(&self, rel: &str) -> Option<String> {
        match self {
            Site::Local => None,
            Site::Remote { home, .. } => Some(format!("{}/{rel}", home.trim_end_matches('/'))),
        }
    }

    /// Must be absolute — the up script `cd`s into a worktree, so a relative
    /// `COMPOSE_FILE` would resolve against that cwd and double up.
    pub fn abs_base(&self, base: &str) -> String {
        match self {
            Site::Local => base.to_string(),
            Site::Remote { .. } if base.starts_with('/') => base.to_string(),
            Site::Remote { .. } => self.home_path(base).expect("remote site carries a home"),
        }
    }

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

    /// For pipelines and builtins where [`command`](Self::command)'s per-arg quoting would fight the shell.
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

    /// Remote path streams via stdin so payload size/quoting is unbounded.
    pub fn write_file(&self, path: &str, content: &str) -> Result<()> {
        match self {
            Site::Local => {
                if let Some(parent) = std::path::Path::new(path).parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(path, content).with_context(|| format!("write {path}"))
            }
            Site::Remote { connection, .. } => {
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

fn ssh_bin() -> String {
    std::env::var("WS_SSH").unwrap_or_else(|_| "ssh".to_string())
}

/// Fail on password prompt and cap the connect wait; a dead host can't stall the caller.
const SSH_FAST_OPTS: &[&str] = &["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"];

/// Collapses N TCP+auth handshakes into one; `%C` in `ControlPath` keeps
/// parallel `ws` to different servers from sharing a master.
const SSH_MUX_OPTS: &[&str] = &[
    "-o", "ControlMaster=auto",
    "-o", "ControlPath=~/.ssh/ws-cm-%C",
    "-o", "ControlPersist=30",
    // Silences ssh's `Shared connection to <host> closed.` line on stderr.
    "-o", "LogLevel=ERROR",
];

fn ssh_fast(connection: &str) -> Command {
    let mut c = Command::new(ssh_bin());
    c.args(SSH_FAST_OPTS).args(SSH_MUX_OPTS).arg(connection);
    c
}

/// Blocks argument injection (CWE-88, scp/git CVE-2017-1000117) from a hostile `connection` in a cloned `.ws.json`.
/// Defense-in-depth against SSH arg injection — rejects `-…` (option-shaped) and control chars (terminal escapes).
pub fn connection_looks_safe(connection: &str) -> bool {
    !connection.is_empty()
        && !connection.starts_with('-')
        && !connection.chars().any(char::is_control)
}

/// Refusal wrapper for user-supplied hosts; `config::load` keeps its own wording.
pub fn ensure_connection_safe(connection: &str) -> anyhow::Result<()> {
    if connection.is_empty() {
        anyhow::bail!("an SSH host must not be empty");
    }
    if !connection_looks_safe(connection) {
        anyhow::bail!(
            "unsafe SSH host `{connection}` — must not start with `-` (ssh would read it as an option)"
        );
    }
    Ok(())
}

/// A passphrase-protected key with no agent reads as unreachable — that's what `--no-verify` is for.
pub fn reachable(connection: &str) -> bool {
    if !connection_looks_safe(connection) {
        return false;
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

/// Transport-agnostic client-reachable address via `ssh -G` + DNS; `-G` never
/// connects, so safe on an unreachable server.
pub fn routing_address(connection: &str) -> Option<String> {
    if !connection_looks_safe(connection) {
        return None;
    }
    let out = Command::new(ssh_bin())
        .arg("-G")
        .arg(connection)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let host = ssh_g_hostname(&String::from_utf8_lossy(&out.stdout))?;
    resolve_to_ip(&host)
}

fn ssh_g_hostname(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix("hostname ").map(str::trim))
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// IPv4 wins on dual-stack — the less surprising value in a `curl --resolve`.
fn resolve_to_ip(host: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = (host, 22u16).to_socket_addrs().ok()?.collect();
    let pick = addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first())?;
    Some(pick.ip().to_string())
}

/// Ships repo-invisible `.ws-hooks/` (which the worktree sync doesn't cover) as
/// `tar | ssh 'tar -x'` with mode bits preserved so `+x` survives.
pub fn push_ws_hooks(site: &Site, ws_root: &Path, base: &str) -> Result<()> {
    let Site::Remote { connection, .. } = site else { return Ok(()) };
    if !ws_root.join(".ws-hooks").is_dir() {
        return Ok(());
    }
    // `--warning=no-unknown-keyword` silences GNU tar's noise about the
    // `com.apple.provenance` xattr headers macOS `tar` stamps in.
    let mut tar = Command::new("tar")
        .args(["-C", &ws_root.to_string_lossy(), "-cf", "-", ".ws-hooks"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn tar for the .ws-hooks push")?;
    let archive = tar.stdout.take().expect("tar stdout was piped");
    // Restore `+x` on shebanged files (a client-side editor may have dropped it),
    // leaving `.sql` seeds and sourced `lib/` fragments alone. Trailing `exit 0`
    // so the loop's last `grep` miss doesn't fail the whole chain.
    let remote = format!(
        "mkdir -p {b} && tar -C {b} --warning=no-unknown-keyword -xpf - && \
         find {b}/.ws-hooks -type f -exec sh -c \
         'for f do head -c2 \"$f\" | grep -q \"^#!\" && chmod +x \"$f\"; done; exit 0' _ {{}} +",
        b = shell_quote(base)
    );
    let out = ssh_command(connection, &remote, false)
        .stdin(Stdio::from(archive))
        .output()
        .context("ssh for the .ws-hooks push")?;
    let _ = tar.wait();
    if !out.status.success() {
        bail!(
            "couldn't push .ws-hooks to {connection}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `script` must be read-only. `None` means SSH couldn't connect (vs an empty
/// `Some("")` for a successful but silent script).
pub fn ssh_capture(connection: &str, script: &str) -> Option<String> {
    if !connection_looks_safe(connection) {
        return None;
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

/// The reply is only trusted after this line — an interactive rc's prompt/MOTD chatter is not the answer.
const PROBE_SENTINEL: &str = "__WS_PATH_PROBE__";

/// A misbehaving rc must degrade to "no harvested PATH" rather than hang the op.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// In-process only, so a run never sees a stale value; no invalidation to get wrong.
static LOGIN_PATH: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

/// The user's terminal `PATH` so version-manager shims resolve in
/// non-interactive SSH. Interactive not login (nvm installs to
/// `.zshrc`/`.bashrc`); only `PATH` is taken so we don't clobber
/// forwarded `SSH_AUTH_SOCK`.
pub fn login_path(connection: &str) -> Option<String> {
    let cache = LOGIN_PATH.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(connection) {
        return hit.clone();
    }
    let probed = probe_login_path(connection);
    cache.lock().unwrap().insert(connection.to_string(), probed.clone());
    probed
}

/// Stderr dropped because an interactive shell without a tty legitimately complains about job control.
fn probe_login_path(connection: &str) -> Option<String> {
    if !connection_looks_safe(connection) {
        return None;
    }
    let script = format!(
        "exec \"${{SHELL:-/bin/sh}}\" -ic 'printf \"%s\\n%s\\n\" {PROBE_SENTINEL} \"$PATH\"'"
    );
    let child = ssh_command(connection, &script, false)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    parse_probe(&capture_within(child, PROBE_TIMEOUT)?)
}

/// Reads stdout on a helper thread so a full pipe buffer can't deadlock the wait loop.
fn capture_within(mut child: std::process::Child, timeout: Duration) -> Option<String> {
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader.join().unwrap_or_default();
                return status.success().then_some(text);
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

fn parse_probe(out: &str) -> Option<String> {
    let mut lines = out.lines();
    lines.find(|l| l.trim() == PROBE_SENTINEL)?;
    let path = lines.next()?.trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Mirrors local `.worktrees/<task>/<repo>` minus the prefix; server-side these are plain synced dirs.
pub fn worktree_path(workspace_path: &str, task: &str, repo: &str) -> String {
    format!("{}/{task}/{repo}", workspace_path.trim_end_matches('/'))
}

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

/// POSIX shell *name* (`[A-Za-z_][A-Za-z0-9_]*`) — anything else is a command word, not an assignment.
fn is_shell_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

struct EnvPrefix {
    words: String,
    /// `env` is a command, not a builtin, so `exec` must come before it (`exec env K=V cmd`).
    via_env: bool,
}

/// A key like `WS_PORT_redis-cache` (from a dashed service) isn't a shell name, so bare `K=V` becomes the command word — remote script exits 127 while local works fine (`Command::env` accepts any name). Hand such pairs to `env` with each `K=V` shell-quoted, same shape `supervise::up` already uses. Also closes an injection: a key with `;` in bare form ran attacker code on the server.
fn env_prefix(env: &[(&str, &str)]) -> EnvPrefix {
    let via_env = env.iter().any(|(k, _)| !is_shell_name(k));
    let mut words = if via_env { String::from("env ") } else { String::new() };
    for (k, v) in env {
        if via_env {
            words.push_str(&format!("{} ", shell_quote(&format!("{k}={v}"))));
        } else {
            words.push_str(&format!("{k}={} ", shell_quote(v)));
        }
    }
    EnvPrefix { words, via_env }
}

/// `cd && …` short-circuits so a missing worktree surfaces as non-zero, not a silent no-op.
pub fn remote_script(dir: &str, env: &[(&str, &str)], command: &[String]) -> String {
    let prefix = env_prefix(env);
    let quoted: Vec<String> = command.iter().map(|a| shell_quote(a)).collect();
    let cmd = quoted.join(" ");
    let dir = shell_quote(dir);
    if prefix.via_env {
        format!("cd {dir} && exec {}{cmd}", prefix.words)
    } else {
        format!("cd {dir} && {}exec {cmd}", prefix.words)
    }
}

/// Remote analogue of [`crate::hooks`]; `WS_WORKSPACE_ROOT` is the synced-worktrees base
/// (no `.ws.json` root on the server). Caller flushes the mutagen sync first.
pub fn run_event(
    connection: &str,
    worktree_base: &str,
    task: &str,
    repo: &str,
    event: &str,
    command: &str,
    extra_env: &[(String, String)],
) -> Result<std::process::ExitStatus> {
    let dir = worktree_path(worktree_base, task, repo);
    let mut env: Vec<(&str, &str)> = vec![
        ("WS_REPO", repo),
        ("WS_BRANCH_SANITIZED", task),
        ("WS_HOOK_NAME", event),
        ("WS_WORKTREE", &dir),
        ("WS_WORKSPACE_ROOT", worktree_base),
    ];
    env.extend(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    // A repo that declares its own PATH still wins over the harvested one.
    let harvested = login_path(connection);
    if let Some(p) = &harvested {
        if !extra_env.iter().any(|(k, _)| k == "PATH") {
            env.push(("PATH", p));
        }
    }
    let cmd = ["sh".to_string(), "-c".to_string(), command.to_string()];
    let script = remote_script(&dir, &env, &cmd);
    // Pipe stdio so remote hook output rides the local gutter/spinner path
    // rather than writing over the step spinner.
    let child = ssh_command(connection, &script, false)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn ssh to {connection}"))?;
    crate::hooks::stream_and_wait(child, &format!("{repo}-{event}"))
}

/// Remote analogue of `hooks::capture_command`; server-side `timeout` so a wedged probe can't stall `ws list`.
pub fn capture_status(
    connection: &str,
    base: &str,
    task: &str,
    repo: &str,
    extra_env: &[(String, String)],
    command: &str,
    timeout_secs: u64,
) -> Option<String> {
    let dir = worktree_path(base, task, repo);
    let mut env: Vec<(&str, &str)> = vec![
        ("WS_REPO", repo),
        ("WS_BRANCH_SANITIZED", task),
        ("WS_HOOK_NAME", "status"),
        ("WS_WORKTREE", &dir),
        ("WS_WORKSPACE_ROOT", base),
    ];
    env.extend(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    // Same env-word rule as `remote_script` (see `env_prefix`): a key that isn't a
    // shell name — `WS_PORT_redis-cache` from a dashed compose service — would be
    // run as a command instead of set, and the failure is silent here, since
    // `ssh_capture` turns the non-zero exit into `None` and `ws list` just shows a
    // dash forever.
    let script = format!(
        "cd {} && {}timeout {timeout_secs} sh -c {}",
        shell_quote(&dir),
        env_prefix(&env).words,
        shell_quote(command)
    );

    let out = ssh_capture(connection, &script)?;
    out.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from)
}

/// Caller sets `want_tty` when its own stdio is interactive.
pub fn ssh_command(connection: &str, script: &str, want_tty: bool) -> Command {
    let mut c = Command::new(ssh_bin());
    if want_tty {
        c.arg("-t");
    }
    c.args(SSH_MUX_OPTS).arg(connection).arg(script);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_g_hostname_reads_the_effective_host() {
        let out = "user hbackman\nhostname srv.tail1234.ts.net\nport 22\n";
        assert_eq!(
            ssh_g_hostname(out).as_deref(),
            Some("srv.tail1234.ts.net")
        );
        // `hostkeyalias` must not be mistaken for `hostname`.
        assert_eq!(ssh_g_hostname("hostkeyalias other\nhostname real.host\n").as_deref(), Some("real.host"));
        assert_eq!(ssh_g_hostname("port 22\n"), None);
        assert_eq!(ssh_g_hostname("hostname \n"), None);
    }

    #[test]
    fn resolve_to_ip_passes_literal_addresses_through() {
        assert_eq!(resolve_to_ip("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(resolve_to_ip("::1").as_deref(), Some("::1"));
        // `.invalid` is RFC 2606 reserved so the None case can't flake.
        assert_eq!(resolve_to_ip("ws-no-such-server.invalid"), None);
    }

    #[test]
    fn routing_address_refuses_an_option_shaped_connection() {
        // `ssh -G` also parses `-oProxyCommand=…` and would run it locally.
        assert_eq!(routing_address("-oProxyCommand=touch /tmp/pwned"), None);
        assert_eq!(routing_address(""), None);
    }

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
        assert!(c.get_program().to_string_lossy().ends_with("ssh"));
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
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
        assert_eq!(site.abs_base("myapp"), "/home/u/myapp");
        assert_eq!(site.abs_base("/srv/ws"), "/srv/ws");
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
    fn connection_guard_rejects_empty_options_and_control_chars() {
        assert!(connection_looks_safe("srv"));
        assert!(connection_looks_safe("user@10.0.0.5"));
        assert!(!connection_looks_safe(""));
        assert!(!connection_looks_safe("-oProxyCommand=evil"));
        // Blocks ANSI/OSC smuggling via an echoed hostile connection.
        assert!(!connection_looks_safe("\u{1b}]0;owned\u{7}host"));
        assert!(!connection_looks_safe("host\nname"));

        let empty = ensure_connection_safe("").unwrap_err().to_string();
        assert!(empty.contains("must not be empty"), "{empty}");
        let dash = ensure_connection_safe("-x").unwrap_err().to_string();
        assert!(dash.contains("unsafe SSH host"), "{dash}");
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

    #[test]
    fn remote_script_routes_a_non_name_env_key_through_env() {
        // `WS_PORT_<svc>` carries the compose service name verbatim, so an ordinary
        // `redis-cache` service gives `WS_PORT_redis-cache` — not a shell name. As a
        // bare assignment word the server shell runs it as a *command*, so the whole
        // script died with `WS_PORT_redis-cache=20001: not found` (exit 127) and the
        // hook never ran, on a workspace that works locally.
        let cmd = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];
        let s = remote_script(
            "/w/t/api",
            &[("WS_REPO", "api"), ("WS_PORT_redis-cache", "20001")],
            &cmd,
        );
        assert_eq!(
            s,
            "cd /w/t/api && exec env WS_REPO=api WS_PORT_redis-cache=20001 sh -c 'echo hi'"
        );
    }

    #[test]
    fn remote_script_quotes_a_hostile_env_key() {
        // A declared `env` key from `.ws.json` is untrusted (the file rides along in
        // a clone). Unquoted, its `;` ended the assignment prefix and ran the rest as
        // its own command — which also escapes the `cd`-failure short-circuit.
        let cmd = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];
        let s = remote_script("/w/t/api", &[("X; touch /tmp/ws-pwned; Y", "1")], &cmd);
        assert_eq!(s, "cd /w/t/api && exec env 'X; touch /tmp/ws-pwned; Y=1' sh -c 'echo hi'");
    }

    #[test]
    fn shell_name_check_matches_the_shell() {
        assert!(is_shell_name("WS_REPO"));
        assert!(is_shell_name("_x9"));
        assert!(!is_shell_name(""));
        assert!(!is_shell_name("9lives")); // can't start with a digit
        assert!(!is_shell_name("WS_PORT_redis-cache"));
        assert!(!is_shell_name("X; touch /tmp/ws-pwned; Y"));
    }

    #[test]
    fn probe_reply_survives_a_chatty_rc() {
        let out = format!(
            "[p10k] instant prompt\nNow using node v22.22.2\n{PROBE_SENTINEL}\n/home/u/.nvm/versions/node/v22.22.2/bin:/usr/bin\n"
        );
        assert_eq!(
            parse_probe(&out).unwrap(),
            "/home/u/.nvm/versions/node/v22.22.2/bin:/usr/bin"
        );
    }

    #[test]
    fn probe_reply_without_a_sentinel_is_refused() {
        // Guessing at the last line could hand a hook a garbage PATH when a
        // shell exits 0 without reaching the printf.
        assert_eq!(parse_probe("/usr/bin:/bin\n"), None);
        assert_eq!(parse_probe(""), None);
        assert_eq!(parse_probe(&format!("{PROBE_SENTINEL}\n")), None);
        assert_eq!(parse_probe(&format!("{PROBE_SENTINEL}\n   \n")), None);
    }
}
