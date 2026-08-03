//! [`Site`] and the SSH plumbing behind it. See ARCHITECTURE.md, "Local and
//! remote are one path".

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use std::process::{Command, Stdio};

/// Where a workspace's stack runs. Both variants honour one contract, so a
/// subsystem takes a `&Site` and never branches on local-vs-remote itself.
pub enum Site {
    Local,
    /// `home` is the server's login `$HOME`, resolved once at construction:
    /// server-side paths are shell-quoted, so a `~` in one would never expand.
    Remote { connection: String, home: String },
}

impl Site {
    /// The `$HOME` fetch doubles as an early reachability check, so a dead or
    /// unauthenticated server fails here rather than part-way through bring-up.
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

    pub fn connection(&self) -> Option<&str> {
        match self {
            Site::Local => None,
            Site::Remote { connection, .. } => Some(connection),
        }
    }

    /// The workspace root **as the execution site sees it**. `WS_WORKSPACE_ROOT`
    /// is what a `.ws-hooks/…` path anchors against, so handing back the client's
    /// root for a command running on the server points it at a filesystem that
    /// isn't there. Must agree with [`worktree_at`](Self::worktree_at).
    pub fn root_at(&self, remote: Option<&crate::config::RemoteConfig>, ws_root: &Path) -> String {
        match remote {
            None => ws_root.to_string_lossy().into_owned(),
            Some(r) => self.abs_base(&r.worktree_base(ws_root)),
        }
    }

    pub fn worktree_at(
        &self,
        remote: Option<&crate::config::RemoteConfig>,
        ws_root: &Path,
        local_worktree: &Path,
        branch_sanitized: &str,
        repo: &str,
    ) -> String {
        match remote {
            None => local_worktree.to_string_lossy().into_owned(),
            Some(_) => worktree_path(&self.root_at(remote, ws_root), branch_sanitized, repo),
        }
    }

    /// Resolve a server-side absolute path from a `$HOME`-relative one.
    pub fn home_path(&self, rel: &str) -> Option<String> {
        match self {
            Site::Local => None,
            Site::Remote { home, .. } => Some(format!("{}/{rel}", home.trim_end_matches('/'))),
        }
    }

    /// The server-side worktree base as an **absolute** path. Server compose
    /// paths (`COMPOSE_FILE`, the override) must be absolute: the up script runs
    /// with `cd <worktree>`, so a relative base resolves against that cwd and
    /// doubles up.
    pub fn abs_base(&self, base: &str) -> String {
        match self {
            Site::Local => base.to_string(),
            Site::Remote { .. } if base.starts_with('/') => base.to_string(),
            Site::Remote { .. } => self.home_path(base).expect("remote site carries a home"),
        }
    }

    /// A `Command` that runs `argv` at this site; the caller finalizes stdio.
    pub fn command(&self, argv: &[&str]) -> Command {
        match self {
            Site::Local => {
                let mut c = Command::new(argv[0]);
                c.args(&argv[1..]);
                c
            }
            Site::Remote { .. } => {
                let script: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
                self.shell(&script.join(" "))
            }
        }
    }

    /// A raw shell `script` at this site, for pipelines and builtins where
    /// [`command`](Self::command)'s per-arg quoting would fight the shell. The
    /// script reaches the *server* shell verbatim, so its `$VAR`s expand there.
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

    /// Write `content` to `path` at this site, creating parent dirs.
    pub fn write_file(&self, path: &str, content: &str) -> Result<()> {
        match self {
            Site::Local => {
                if let Some(parent) = std::path::Path::new(path).parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(path, content).with_context(|| format!("write {path}"))
            }
            Site::Remote { connection, .. } => {
                // Stream on stdin rather than embedding: no size or quoting limit
                // on the payload.
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

    /// Best-effort: teardown drops the generated compose override with this, and
    /// an already-missing file is a success.
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

/// `WS_SSH` lets tests substitute a fake that records its argv.
fn ssh_bin() -> String {
    std::env::var("WS_SSH").unwrap_or_else(|_| "ssh".to_string())
}

/// For probes only: fail rather than prompt, and cap the wait so a dead host
/// can't stall the caller.
const SSH_FAST_OPTS: &[&str] = &["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"];

/// Applied to every `ws` SSH call: one op makes many short connections, and
/// multiplexing collapses their handshakes onto one master. `%C` (user+host+port)
/// keys the socket so parallel `ws` to *different* servers can't share a master.
const SSH_MUX_OPTS: &[&str] = &[
    "-o", "ControlMaster=auto",
    "-o", "ControlPath=~/.ssh/ws-cm-%C",
    "-o", "ControlPersist=30",
    // Suppresses multiplexing's INFO-level `Shared connection to <host> closed.`,
    // which otherwise trails the output of every command the user ran. Connection
    // and host-key failures are ERROR and still surface.
    "-o", "LogLevel=ERROR",
];

/// The one owner of the `ssh [-t] [<fast-opts>] <mux-opts> <dest>` prefix; the
/// caller appends the remote command and sets stdio.
///
/// `batch` belongs on a probe, which must fail rather than hang, but *not* on a
/// call that may legitimately have to prompt for SSH auth — `BatchMode=yes`
/// turns an answerable passphrase prompt into a refusal.
fn ssh_base(connection: &str, want_tty: bool, batch: bool) -> Command {
    let mut c = Command::new(ssh_bin());
    if want_tty {
        c.arg("-t");
    }
    if batch {
        c.args(SSH_FAST_OPTS);
    }
    c.args(SSH_MUX_OPTS).arg(connection);
    c
}

fn ssh_fast(connection: &str) -> Command {
    ssh_base(connection, false, true)
}

/// Whether an SSH destination is safe to pass as an `ssh` argv element, and the
/// one owner of that question.
///
/// `ssh` parses any argument beginning with `-` as an **option**, so a
/// `connection` of `-oProxyCommand=…` — from a hostile `.ws.json` riding along in
/// a clone — makes `ssh` run a local command before connecting (CWE-88, the
/// CVE-2017-1000117 class). `--` is not a reliable guard for `ssh`, so the value
/// is validated instead.
pub fn connection_looks_safe(connection: &str) -> bool {
    // Control chars are illegitimate in a host/alias anyway, and rejecting them
    // stops a stored connection smuggling ANSI/OSC sequences into the lines that
    // echo it.
    !connection.is_empty()
        && !connection.starts_with('-')
        && !connection.chars().any(char::is_control)
}

/// [`connection_looks_safe`] as a refusal, for hosts arriving from user input.
/// `config::load` keeps its own wording — a *stored* connection's threat model
/// deserves the extra context.
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

/// Light reachability probe, so `ws init --remote` catches a bad connection at
/// config time rather than at first `ws new`. A passphrase-protected key with no
/// agent reads as unreachable — that is what `--no-verify` is for.
pub fn reachable(connection: &str) -> bool {
    ssh_capture(connection, "true").is_some()
}

/// The address a **client** can reach the offload server at, derived from the SSH
/// connection alone.
///
/// Deliberately **transport-agnostic**: if you can SSH to the box you already have
/// a routable address for it, so a LAN IP, a public IP, an `~/.ssh/config` alias
/// and Tailscale MagicDNS all arrive through this one path. Tailscale is a *case
/// that works*, never a code path — `ws` takes no position on network topology.
///
/// `-G` only parses config, it never connects, so this is safe to call on an
/// unreachable server. `None` when it fails or the hostname doesn't resolve; the
/// caller turns that into "set `remote.publish_address`" rather than guessing.
pub fn routing_address(connection: &str) -> Option<String> {
    if !connection_looks_safe(connection) {
        return None; // defense-in-depth: never hand ssh an option-shaped host
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

/// The host `ssh` would actually dial: `-G` prints one lowercased
/// `<keyword> <value>` per line, with any `~/.ssh/config` alias already resolved.
fn ssh_g_hostname(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix("hostname ").map(str::trim))
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// IPv4 wins when a name resolves to both — a tailnet name gives a `100.x` and an
/// `fd7a:` address, and the v4 is the less surprising thing to write into a
/// resolver entry and a `curl --resolve`.
fn resolve_to_ip(host: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    // The port is irrelevant to resolution; `to_socket_addrs` just demands one.
    let addrs: Vec<_> = (host, 22u16).to_socket_addrs().ok()?.collect();
    let pick = addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first())?;
    Some(pick.ip().to_string())
}

/// Push the workspace-root `.ws-hooks/` to the server. These scripts live
/// *outside* the managed repos, so the worktree sync never carries them, yet a
/// hook running server-side resolves them via `$WS_WORKSPACE_ROOT/.ws-hooks/…`.
///
/// `tar | ssh 'tar -x'` because it preserves mode bits, and a hook that loses
/// its `+x` is a hook `sh -c "<path>"` can't run. `base` is the absolute server
/// worktree base, so the directory lands at `<base>/.ws-hooks/`.
pub fn push_ws_hooks(site: &Site, ws_root: &Path, base: &str) -> Result<()> {
    let Site::Remote { connection, .. } = site else { return Ok(()) };
    if !ws_root.join(".ws-hooks").is_dir() {
        return Ok(());
    }
    // The push isn't a hook, so its output shouldn't stream into one: `tar` stderr
    // is dropped and the ssh side only surfaces on failure.
    let mut tar = Command::new("tar")
        .args(["-C", &ws_root.to_string_lossy(), "-cf", "-", ".ws-hooks"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn tar for the .ws-hooks push")?;
    let archive = tar.stdout.take().expect("tar stdout was piped");
    // `-xpf` preserves the mode faithfully, which is the problem: a hook an editor
    // rewrote without `+x` lands 644 and fails at hook time as a bare `Permission
    // denied`. A shebang is the file's own claim to be executable, so it decides
    // rather than a blanket `chmod -R +x` — a `.sql` seed or a sourced `lib/`
    // fragment stays untouched. The tree is a ws-managed copy, so repairing it
    // asserts nothing about the user's own files. `exit 0` because the loop's last
    // `grep` miss would otherwise fail the chain; `--warning=no-unknown-keyword`
    // because GNU tar objects, harmlessly, to the xattr headers macOS `tar` stamps in.
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

/// Run a `script` on `connection` and capture stdout. The contract is that
/// `script` is **read-only** — it never installs, mutates, or `sudo`s.
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

/// An interactive rc is entitled to print (prompt frameworks, `nvm use` chatter,
/// MOTD-alikes), so the reply is only trusted after this line.
const PROBE_SENTINEL: &str = "__WS_PATH_PROBE__";

/// A misbehaving rc that blocks forever must degrade to "no harvested PATH",
/// never hang the op.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Cached because the probe costs ~1s of rc evaluation and one `ws up` dispatches
/// several hooks. In-process only, so there is no stale-value problem to get wrong.
static LOGIN_PATH: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

/// The `PATH` a hook would see if the user ran it in their own terminal on
/// `connection`.
///
/// Version managers (nvm, asdf, pyenv, mise) are rc-installed shell functions,
/// so nothing `ws` could detect would find them — but hooks dispatch over
/// non-interactive SSH, where `npm ci` then fails with `npm: not found` on a
/// server where `npm` is plainly present. Asking the user's shell is the only
/// way to know.
///
/// **Interactive**, not login: nvm's installer appends to `.zshrc`/`.bashrc`,
/// which a login shell doesn't source, so `-l` misses the common case entirely.
///
/// Only `PATH` is taken. A full env delta would import the probe shell's
/// `SSH_AUTH_SOCK` over the forwarded agent's, breaking any hook that
/// authenticates.
///
/// Known limit: `nvm` *the command* still won't resolve in a hook — it is a shell
/// function, and functions don't survive an environment. The binaries it puts on
/// `PATH` do.
pub fn login_path(connection: &str) -> Option<String> {
    let cache = LOGIN_PATH.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(connection) {
        return hit.clone();
    }
    let probed = probe_login_path(connection);
    cache.lock().unwrap().insert(connection.to_string(), probed.clone());
    probed
}

/// The one owner of "harvested login `PATH` unless the caller declared one",
/// shared by [`run_event`] and `supervise::up`. A local site (`connection` of
/// `None`) harvests nothing — a local hook already inherits the user's
/// environment. See [`login_path`] for why this exists at all.
pub fn harvested_path(
    connection: Option<&str>,
    declared: &[(String, String)],
) -> Option<(String, String)> {
    let harvested = login_path(connection?)?;
    (!declared.iter().any(|(k, _)| k == "PATH")).then(|| ("PATH".to_string(), harvested))
}

/// `$SHELL -i` so rc files run; null stdin so a shell that reads it sees EOF
/// instead of blocking; stderr dropped because an interactive shell without a tty
/// legitimately complains about job control.
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
    parse_probe(&crate::hooks::capture_within(child, PROBE_TIMEOUT)?)
}

/// The `PATH` is the line after the sentinel; everything before it is whatever
/// the user's rc decided to print.
fn parse_probe(out: &str) -> Option<String> {
    let mut lines = out.lines();
    lines.find(|l| l.trim() == PROBE_SENTINEL)?;
    let path = lines.next()?.trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Server-side path of a synced worktree. Mirrors the local
/// `.worktrees/<task>/<repo>` layout *without* the `.worktrees` prefix: on the
/// server these are plain directories, not git worktrees.
pub fn worktree_path(workspace_path: &str, task: &str, repo: &str) -> String {
    format!("{}/{task}/{repo}", workspace_path.trim_end_matches('/'))
}

/// POSIX single-quote a shell word. Every value `ws` interpolates into a remote
/// script passes through here — it is the boundary that keeps a hostile one from
/// becoming a command.
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

/// Whether `name` is usable as a bare `K=V` assignment word. Anything else is a
/// *command* word to the shell, not an assignment — see [`env_prefix`].
fn is_shell_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

struct EnvPrefix {
    /// Trailing space included, so a command concatenates directly onto it.
    words: String,
    /// True when `words` leads with the `env` **command**, which `exec` must then
    /// precede (`exec env K=V cmd`): `env` is an ordinary command, not a builtin
    /// the shell would carry a preceding `exec` into.
    via_env: bool,
}

impl EnvPrefix {
    fn exec(&self, command: &str) -> String {
        if self.via_env {
            format!("exec {}{command}", self.words)
        } else {
            format!("{}exec {command}", self.words)
        }
    }
}

/// Render `env` as words that place the pairs in the environment of the command
/// that follows.
///
/// Plain `K=V …` assignment words while every key is a shell name, which is the
/// readable form — but a key is not guaranteed to be one. `WS_PORT_<svc>` carries
/// the compose service name verbatim, so a `redis-cache` service yields
/// `WS_PORT_redis-cache`, and `.ws.json`'s `env` map may hold anything at all. To
/// the shell such a word is the *command*, not an assignment: the script dies at
/// `WS_PORT_redis-cache=20001: not found`, and a key carrying `;` runs its own
/// commands on the server instead. So when any key isn't a shell name, hand the
/// pairs to `env` with each whole `K=V` token `shell_quote`d — `env` accepts
/// arbitrary names, restoring parity with the local `Command::env` path.
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

/// `&&`, not `;`, so a missing worktree surfaces as the shell's non-zero exit
/// rather than running `rest` in the wrong directory.
fn in_dir(dir: &str, rest: &str) -> String {
    format!("cd {} && {rest}", shell_quote(dir))
}

/// The remote shell script run over SSH: `cd` into `dir`, export the `WS_*` env
/// pairs, `exec` the command.
pub fn remote_script(dir: &str, env: &[(&str, &str)], command: &[String]) -> String {
    let quoted: Vec<String> = command.iter().map(|a| shell_quote(a)).collect();
    in_dir(dir, &env_prefix(env).exec(&quoted.join(" ")))
}

/// [`remote_script`] for a read-only `status` probe, bounded by a server-side
/// `timeout` and deliberately *not* `exec`ed — `timeout` has to stay alive to
/// enforce the bound.
fn status_script(dir: &str, env: &[(&str, &str)], command: &str, timeout_secs: u64) -> String {
    in_dir(
        dir,
        &format!(
            "{}timeout {timeout_secs} sh -c {}",
            env_prefix(env).words,
            shell_quote(command)
        ),
    )
}

/// The one owner of the remote side of the `WS_*` contract — both the lifecycle
/// dispatch and the `status` probe build their env here, so a new key can't reach
/// one and miss the other. Injected pairs land last, so a declared `env` key
/// overrides a base one.
fn event_env(
    base: &str,
    dir: &str,
    task: &str,
    repo: &str,
    event: &str,
    extra_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env = crate::hooks::base_env(Path::new(base), dir, repo, task, event);
    env.extend(extra_env.iter().cloned());
    env
}

fn as_pairs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

/// Run a lifecycle command on the server, mirroring its exit status. The caller
/// must flush the worktree's mutagen sync first, or the server runs against stale
/// files.
///
/// The `WS_*` keys are the local contract verbatim but the paths are the
/// server's: `WS_WORKSPACE_ROOT` is the synced-worktrees base, since there is no
/// `.ws.json` workspace root on the server.
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
    let mut env = event_env(worktree_base, &dir, task, repo, event, extra_env);
    // After `extra_env`, though it makes no difference today: `harvested_path`
    // already yields nothing when the repo declared its own PATH.
    env.extend(harvested_path(Some(connection), extra_env));
    let cmd = ["sh".to_string(), "-c".to_string(), command.to_string()];
    let script = remote_script(&dir, &as_pairs(&env), &cmd);
    // Piped rather than inherited so the output goes through the same `hook_line`
    // path a local hook uses; inherited, the server's output writes straight over
    // the step spinner.
    let child = ssh_command(connection, &script, false)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn ssh to {connection}"))?;
    crate::hooks::stream_and_wait(child, &format!("{repo}-{event}"))
}

/// The remote analogue of `hooks::capture_command`: a repo's read-only `status`
/// command, bounded by a server-side `timeout` so a wedged probe can't stall
/// `ws list`. `base` is the absolute server worktree base.
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
    // No harvested login PATH, unlike `run_event`: the harvest costs an extra SSH
    // round trip on a cold cache, and this runs once per repo behind `ws list`.
    let env = event_env(base, &dir, task, repo, "status", extra_env);
    let script = status_script(&dir, &as_pairs(&env), command, timeout_secs);

    let out = ssh_capture(connection, &script)?;
    crate::ui::first_nonempty_line(&out).map(String::from)
}

/// `want_tty` forces a remote pty, which the caller decides from whether our own
/// stdio is interactive: a tinker session or a prompt wants one, a piped or
/// redirected invocation gets cleaner output without.
///
/// Not `batch` (see [`ssh_base`]): these calls may need to prompt for SSH auth.
pub fn ssh_command(connection: &str, script: &str, want_tty: bool) -> Command {
    let mut c = ssh_base(connection, want_tty, false);
    c.arg(script);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_g_hostname_reads_the_effective_host() {
        // Real `ssh -G` shape: lowercased keyword, space, value.
        let out = "user alice\nhostname srv.tail1234.ts.net\nport 22\n";
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
        // A connection that's already an IP needs no DNS — and must survive the
        // round trip unchanged, since it becomes the client's wildcard answer.
        assert_eq!(resolve_to_ip("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(resolve_to_ip("::1").as_deref(), Some("::1"));
        // RFC 2606 reserves `.invalid`: never resolves, so this can't flake.
        assert_eq!(resolve_to_ip("ws-no-such-server.invalid"), None);
    }

    #[test]
    fn routing_address_refuses_an_option_shaped_connection() {
        // The argument-injection guard must hold on this path too — `ssh -G`
        // parses `-oProxyCommand=…` and would run it locally.
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
        // WS_SSH is unset in the test process → the program is `ssh`.
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
        // The default worktree base is home-relative → resolved against $HOME so
        // `COMPOSE_FILE` doesn't double-resolve under the `cd`'d worktree.
        assert_eq!(site.abs_base("myapp"), "/home/u/myapp");
        assert_eq!(site.abs_base("/srv/ws"), "/srv/ws");
        assert_eq!(Site::Local.abs_base("myapp"), "myapp");
    }

    #[test]
    fn site_paths_are_the_execution_sites_own() {
        let root = Path::new("/home/me/myapp");
        let wt = Path::new("/home/me/myapp/.worktrees/feat-foo/api");
        assert_eq!(Site::Local.root_at(None, root), "/home/me/myapp");
        assert_eq!(
            Site::Local.worktree_at(None, root, wt, "feat-foo", "api"),
            wt.to_string_lossy()
        );
        // Remote: the server's own paths, never the client's, which don't exist
        // there.
        let site = Site::Remote { connection: "srv".into(), home: "/home/u".into() };
        let r = crate::config::RemoteConfig::new("srv".into(), Some("ws".into()));
        assert_eq!(site.root_at(Some(&r), root), "/home/u/ws");
        assert_eq!(
            site.worktree_at(Some(&r), root, wt, "feat-foo", "api"),
            "/home/u/ws/feat-foo/api"
        );
    }

    #[test]
    fn a_remote_command_gets_the_same_ws_contract_as_a_local_hook() {
        // Injected pairs land last, so a declared `env` key overrides a base one.
        let env = event_env(
            "/home/u/ws",
            "/home/u/ws/feat-foo/api",
            "feat-foo",
            "api",
            "up",
            &[("WS_PORT_web".to_string(), "20001".to_string())],
        );
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "WS_WORKSPACE_ROOT",
                "WS_WORKTREE",
                "WS_REPO",
                "WS_BRANCH_SANITIZED",
                "WS_HOOK_NAME",
                "WS_PORT_web"
            ]
        );
        assert_eq!(env[0].1, "/home/u/ws");
        assert_eq!(env[1].1, "/home/u/ws/feat-foo/api");
        assert_eq!(env[4].1, "up");
    }

    #[test]
    fn status_script_bounds_the_probe_without_exec() {
        // `timeout` must stay alive to enforce the bound, so this script is the one
        // remote form that doesn't `exec`.
        let s = status_script("/w/t/api", &[("WS_REPO", "api")], "docker compose ps", 5);
        assert_eq!(s, "cd /w/t/api && WS_REPO=api timeout 5 sh -c 'docker compose ps'");
        // The `env_prefix` rule holds here too, where the failure is silent:
        // `ssh_capture` turns the non-zero exit into `None` and `ws list` shows a
        // dash forever.
        let s = status_script("/w/t/api", &[("WS_PORT_redis-cache", "20001")], "ps", 5);
        assert_eq!(s, "cd /w/t/api && env WS_PORT_redis-cache=20001 timeout 5 sh -c ps");
    }

    #[test]
    fn a_local_site_harvests_no_login_path() {
        // Nothing to harvest: a local hook already inherits the user's environment.
        assert_eq!(harvested_path(None, &[]), None);
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
        // ANSI/OSC smuggling via an echoed hostile connection.
        assert!(!connection_looks_safe("\u{1b}]0;owned\u{7}host"));
        assert!(!connection_looks_safe("host\nname"));

        // The refusal wrapper distinguishes empty from option-shaped.
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
        // `WS_PORT_redis-cache` isn't a shell name, so as a bare assignment word the
        // server shell would run it as a *command* and the hook would never run.
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
        // A declared `env` key from `.ws.json` is untrusted — the file rides along in
        // a clone. Unquoted, its `;` ends the assignment prefix and runs the rest as
        // its own command, escaping the `cd`-failure short-circuit too.
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
        // Anything ahead of the sentinel is the user's shell talking — a prompt
        // banner, `nvm use` chatter — not the answer.
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
        // A shell that failed before reaching the printf still exits 0 in some
        // configurations, so guessing at the last line would hand a hook a garbage
        // PATH.
        assert_eq!(parse_probe("/usr/bin:/bin\n"), None);
        assert_eq!(parse_probe(""), None);
        // Sentinel present but nothing after it (rc killed mid-write).
        assert_eq!(parse_probe(&format!("{PROBE_SENTINEL}\n")), None);
        assert_eq!(parse_probe(&format!("{PROBE_SENTINEL}\n   \n")), None);
    }
}
