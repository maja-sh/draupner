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

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
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

    /// The SSH destination this site runs through, `None` for `Local`. Lets a
    /// caller reach the things that only exist for a connection (the harvested
    /// login `PATH`) without matching on the variant itself.
    pub fn connection(&self) -> Option<&str> {
        match self {
            Site::Local => None,
            Site::Remote { connection, .. } => Some(connection),
        }
    }

    /// The workspace root **as the execution site sees it**: the local root for a
    /// local workspace, else the server's synced-worktree base, absolute
    /// (home-resolved — see [`abs_base`](Self::abs_base)).
    ///
    /// This and [`worktree_at`](Self::worktree_at) are the one owner of "which
    /// paths does a command running at this site see", and they must agree:
    /// `WS_WORKSPACE_ROOT` is what a `.ws-hooks/…` script path anchors against, so
    /// handing back the client's root while the command runs on the server points
    /// it at a filesystem that isn't there (`/Users/…: not found` on a Linux box).
    /// Absolute for the same reason `COMPOSE_FILE` must be: the command runs with
    /// `cd <worktree>`, so a relative base would resolve against that cwd.
    pub fn root_at(&self, remote: Option<&crate::config::RemoteConfig>, ws_root: &Path) -> String {
        match remote {
            None => ws_root.to_string_lossy().into_owned(),
            Some(r) => self.abs_base(&r.worktree_base(ws_root)),
        }
    }

    /// A repo's worktree at the execution site: `local_worktree` for a local
    /// workspace, else the server-side synced dir under [`root_at`](Self::root_at).
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
            Site::Remote { .. } => {
                let script: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
                self.shell(&script.join(" "))
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

/// Connection-multiplexing options applied to every `ws` SSH call. A single op
/// makes many short connections (Caddy bring-up, per-port probes, route CRUD,
/// the lifecycle script); `ControlMaster=auto` opens a master on the first
/// connect and the rest ride it, collapsing N TCP+auth handshakes into one.
/// `ControlPersist` keeps the master briefly so consecutive `ws` commands reuse
/// it too; `ControlPath` is keyed by `%C` (a hash of user+host+port) so parallel
/// `ws` to *different* servers never share a master. `ssh` re-creates a stale
/// socket automatically, so a crashed master self-heals.
const SSH_MUX_OPTS: &[&str] = &[
    "-o", "ControlMaster=auto",
    "-o", "ControlPath=~/.ssh/ws-cm-%C",
    "-o", "ControlPersist=30",
    // Multiplexing's own parting words. Every `ws exec` ended with `Shared
    // connection to <host> closed.` on stderr — ssh narrating its bookkeeping over
    // the output of the command the user actually ran. It is logged at INFO, so
    // raising the floor to ERROR drops it while leaving anything worth acting on:
    // verified by hand that `Host key verification failed.` and `connect to host …
    // Connection timed out` both still surface at this level.
    "-o", "LogLevel=ERROR",
];

/// The single owner of the `ssh [-t] [<fast-opts>] <mux-opts> <dest>` prefix
/// shared by `Site`, the probes, and the lifecycle dispatch — the caller appends
/// the remote command/script and sets stdio.
///
/// `want_tty` forces a remote pty (`-t`). `batch` applies [`SSH_FAST_OPTS`]: it
/// belongs on a probe, which must fail rather than hang, but *not* on a call that
/// may legitimately have to prompt for SSH auth (a key passphrase with no agent,
/// a password) — there `BatchMode=yes` turns an answerable prompt into a refusal.
fn ssh_base(connection: &str, want_tty: bool, batch: bool) -> Command {
    let mut c = Command::new(ssh_bin());
    if want_tty {
        c.arg("-t");
    }
    if batch {
        c.args(SSH_FAST_OPTS);
    }
    // Ride the same multiplexed connection the probes/`Site` calls opened, so a
    // later call doesn't pay a second full handshake.
    c.args(SSH_MUX_OPTS).arg(connection);
    c
}

/// An `ssh` command to `connection` with the non-interactive fail-fast options
/// (+ connection multiplexing) applied — the caller appends the remote
/// command/script and sets stdio.
fn ssh_fast(connection: &str) -> Command {
    ssh_base(connection, false, true)
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
    // Control chars can't appear in a legitimate host/alias; rejecting them
    // also keeps a hostile stored connection from smuggling ANSI/OSC sequences
    // into terminal output when it's echoed (header/note lines).
    !connection.is_empty()
        && !connection.starts_with('-')
        && !connection.chars().any(char::is_control)
}

/// `connection_looks_safe` as a refusal with the one shared message, for the
/// places a host enters from user input (`ws remote set`, `ws remote check
/// --host`, `ws init --remote`, `ws config set remote.host`). `config::load`
/// keeps its own wording — a stored connection's threat model (a hostile
/// `.ws.json` in a clone) deserves the extra context.
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

/// The address a **client** can reach the offload server at, derived from the SSH
/// connection alone: `ssh -G` applies `~/.ssh/config` (so an alias, `User`,
/// `Hostname` and `ProxyJump` rewrites are all honoured) and reports the real
/// `hostname` it would dial, which a DNS lookup turns into an IP.
///
/// Deliberately **transport-agnostic**: if you can SSH to the box you already have
/// a routable address for it, so a LAN IP, a public IP, an `~/.ssh/config` alias
/// and Tailscale MagicDNS all arrive through this one path. Tailscale is a *case
/// that works*, never a code path — `ws` takes no position on the user's network
/// topology.
///
/// `-G` only parses config, it never connects, so this is cheap and safe to call
/// on an unreachable server. IPv4 is preferred when a name resolves to both (a
/// tailnet name gives a `100.x` and an `fd7a:` address; the v4 is the less
/// surprising thing to write into a resolver and a `curl --resolve`). `None` when
/// `ssh -G` fails or the hostname doesn't resolve — the caller turns that into a
/// "set `remote.publish_address`" message rather than guessing.
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

/// The effective `Hostname` from `ssh -G` output, which prints one lowercased
/// `<keyword> <value>` per line. This is the value `ssh` would actually dial, so
/// an `~/.ssh/config` alias has already been rewritten to the real host.
fn ssh_g_hostname(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix("hostname ").map(str::trim))
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// Resolve a hostname (or pass through a literal IP) to a single address string.
/// IPv4 wins when a name resolves to both — a tailnet name gives a `100.x` and an
/// `fd7a:` address, and the v4 is the less surprising thing to write into a
/// resolver entry and a `curl --resolve`.
fn resolve_to_ip(host: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    // Port is irrelevant to resolution, but `to_socket_addrs` demands one.
    let addrs: Vec<_> = (host, 22u16).to_socket_addrs().ok()?.collect();
    let pick = addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first())?;
    Some(pick.ip().to_string())
}

/// Push the workspace-root `.ws-hooks/` to the server so a hook running there can
/// reference `$WS_WORKSPACE_ROOT/.ws-hooks/…` — the lane for shared, repo-invisible
/// hook scripts (a db-seed `up.sh`) that live *outside* the managed repos and
/// therefore never reach the server via the worktree sync.
///
/// One-way (client → server), whole-directory, **mode bits preserved** so an
/// executable script stays executable. Implemented as a `tar | ssh 'tar -x'` pipe:
/// portable, atomic-per-file, and it keeps the `+x` a `sh -c "<path>"` invocation
/// relies on. No-op for a local site or an absent `.ws-hooks/`. `base` is the
/// absolute server worktree base (= `WS_WORKSPACE_ROOT` on the server), so the
/// directory lands at `<base>/.ws-hooks/`.
pub fn push_ws_hooks(site: &Site, ws_root: &Path, base: &str) -> Result<()> {
    let Site::Remote { connection, .. } = site else { return Ok(()) };
    if !ws_root.join(".ws-hooks").is_dir() {
        return Ok(());
    }
    // Quiet plumbing: the push isn't a hook, so its output shouldn't stream into
    // one. `tar` stderr is dropped; the ssh side is captured and only surfaced on
    // failure. `--warning=no-unknown-keyword` on the server extract also silences
    // GNU tar's noise about the `com.apple.provenance` xattr headers macOS `tar`
    // stamps in — cosmetic, the extract succeeds regardless.
    let mut tar = Command::new("tar")
        .args(["-C", &ws_root.to_string_lossy(), "-cf", "-", ".ws-hooks"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn tar for the .ws-hooks push")?;
    let archive = tar.stdout.take().expect("tar stdout was piped");
    // Restore the executable bit on anything with a shebang. `-xpf` preserves the
    // mode faithfully, which is the problem: a hook rewritten on the client by an
    // editor that drops +x lands here 644, and `sh -c '<path>'` fails with a bare
    // `Permission denied` (exit 126) at hook time. The shebang is the file's own
    // declaration that it is meant to be run, so it — not a blanket `chmod -R +x`
    // — decides; a `.sql` seed or a sourced `lib/` fragment stays untouched. The
    // pushed tree is a ws-managed copy, so repairing it asserts nothing about the
    // user's own files. `exit 0` because the loop's last `grep` miss would
    // otherwise fail the whole chain.
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

/// Marks where the probe's own output begins. An interactive rc is entitled to
/// print (prompt frameworks, `nvm use` chatter, MOTD-alikes), so the reply is
/// only trusted after this line.
const PROBE_SENTINEL: &str = "__WS_PATH_PROBE__";

/// How long the login shell gets. A misbehaving rc that blocks forever must
/// degrade to "no harvested PATH", never hang the op.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Cached per connection: harvesting is worth ~1s of interactive-rc evaluation,
/// and a single `ws up` dispatches several hooks. Held in-process only — a run
/// never sees a stale value, so there is no invalidation problem to get wrong.
static LOGIN_PATH: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

/// The `PATH` a hook would see if the user ran it in their own terminal on
/// `connection`, or `None` when it can't be determined.
///
/// A `kind: host` repo gets its toolchain from the user's shell environment,
/// but `ws` dispatches hooks over non-interactive SSH as `sh -c`, where none of
/// it exists — so `npm ci` in a hook fails with `npm: not found` on a server
/// where `npm` is plainly present. Version managers (nvm, asdf, pyenv, mise,
/// volta) are shell-function shims installed by rc files, so no amount of
/// detection in `ws` would find them: the only thing that knows is the user's
/// shell. We ask it, rather than guessing.
///
/// **Interactive**, not login: nvm's own installer appends to `.zshrc`/`.bashrc`,
/// which a login shell does not source — `-l` misses the common case entirely.
///
/// Only `PATH` is taken. A full env delta would import the probe shell's
/// `SSH_AUTH_SOCK` over the forwarded agent's, breaking any hook that
/// authenticates (a private-repo `composer install`), plus session-owned noise
/// like `TERM`/`PWD`/`SHLVL`. `PATH` is where the toolchain actually lives.
///
/// Known limit: `nvm` *the command* still won't resolve in a hook. It is a shell
/// function, and functions don't survive an environment. The binaries it puts on
/// `PATH` — `node`, `npm`, and every shim-based manager — do.
pub fn login_path(connection: &str) -> Option<String> {
    let cache = LOGIN_PATH.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(connection) {
        return hit.clone();
    }
    let probed = probe_login_path(connection);
    cache.lock().unwrap().insert(connection.to_string(), probed.clone());
    probed
}

/// The `PATH` pair to put in front of a command launched at a site, or `None`
/// when there is nothing to add. The one owner of "harvested login PATH unless the
/// caller declared one", shared by the hook dispatch ([`run_event`]) and the
/// detached host-process launch (`supervise::up`) — two paths that build their own
/// command and had this rule spelled out separately.
///
/// `connection` is the site's SSH destination (`None` = local, which harvests
/// nothing: a local hook already inherits the user's environment). Skipped when
/// `declared` carries its own `PATH`, so a repo that sets one still wins. See
/// [`login_path`] for why this exists at all.
pub fn harvested_path(
    connection: Option<&str>,
    declared: &[(String, String)],
) -> Option<(String, String)> {
    let harvested = login_path(connection?)?;
    (!declared.iter().any(|(k, _)| k == "PATH")).then(|| ("PATH".to_string(), harvested))
}

/// One probe round trip. `$SHELL -i` so rc files run; `</dev/null` so a shell
/// that reads stdin sees EOF instead of blocking; stderr dropped because an
/// interactive shell without a tty legitimately complains about job control.
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

/// Pull the `PATH` out of a probe reply: the line after the sentinel. Everything
/// before it is whatever the user's rc decided to print and is discarded.
fn parse_probe(out: &str) -> Option<String> {
    let mut lines = out.lines();
    lines.find(|l| l.trim() == PROBE_SENTINEL)?;
    let path = lines.next()?.trim();
    (!path.is_empty()).then(|| path.to_string())
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

/// Whether `name` is a POSIX shell *name*, i.e. usable as a bare `K=V`
/// assignment word: `[A-Za-z_][A-Za-z0-9_]*`. Anything else is a command word to
/// the shell, not an assignment — see [`env_prefix`].
fn is_shell_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

/// A rendered environment prefix for the remote script builders, plus where
/// `exec` has to go relative to it.
struct EnvPrefix {
    /// The words that put the pairs in the environment of the command that
    /// follows, trailing space included (empty for no pairs).
    words: String,
    /// True when `words` leads with the `env` **command**. `exec` must then come
    /// *before* it (`exec env K=V cmd`): `env` is an ordinary command, not a
    /// builtin the shell would carry a preceding `exec` into.
    via_env: bool,
}

impl EnvPrefix {
    /// `command` with these env words in front of it and `exec` placed where the
    /// shell honours it (see the `via_env` field) — so the login shell
    /// is replaced and the child's exit code propagates back cleanly.
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
/// Plain `K=V …` assignment words while every key is a shell name — the readable
/// form, and what the `WS_*` contract looks like. But a key is not guaranteed to
/// be one. `WS_PORT_<svc>`/`WS_HOST_<svc>` carry the compose service name
/// verbatim, so an ordinary `redis-cache` service yields `WS_PORT_redis-cache`,
/// and the `env` map in `.ws.json` may hold anything at all. A word that isn't a
/// valid assignment is not an assignment to the shell — it is the *command*, so
/// the whole remote script died as `WS_PORT_redis-cache=20001: not found` (exit
/// 127, the hook never ran) on a workspace that works fine locally, where the
/// local path hands the same pairs to `Command::env`, which accepts any name. A
/// key carrying `;` was worse: it broke out of the assignment prefix and ran its
/// own commands on the server, `cd`-failure short-circuit and all.
///
/// So when any key isn't a shell name, hand the pairs to `env` instead, with the
/// whole `K=V` token `shell_quote`d so `env` sees exactly one argument — the rule
/// `supervise::up` already applies for the same reason. `env` accepts arbitrary
/// names, restoring parity with the local path.
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

/// The head every remote script shares: run `rest` in `dir`. `cd` failure
/// short-circuits `&&`, so a missing worktree surfaces as the shell's non-zero
/// exit rather than a silent no-op.
fn in_dir(dir: &str, rest: &str) -> String {
    format!("cd {} && {rest}", shell_quote(dir))
}

/// Build the remote shell script run over SSH: change into `dir`, export the
/// given `WS_*` env pairs, then `exec` the command.
pub fn remote_script(dir: &str, env: &[(&str, &str)], command: &[String]) -> String {
    let quoted: Vec<String> = command.iter().map(|a| shell_quote(a)).collect();
    in_dir(dir, &env_prefix(env).exec(&quoted.join(" ")))
}

/// The remote script for a read-only `status` probe: the same `cd` + env prelude
/// [`remote_script`] builds, but bounded by a server-side `timeout` and *not*
/// `exec`ed — `timeout` has to stay alive to enforce the bound.
///
/// Same env-word rule as `remote_script` (see [`env_prefix`]): a key that isn't a
/// shell name — `WS_PORT_redis-cache` from a dashed compose service — would be run
/// as a command instead of set, and the failure is silent here, since
/// `ssh_capture` turns the non-zero exit into `None` and `ws list` just shows a
/// dash forever.
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

/// The environment a remote command runs with: the shared `WS_*` contract
/// ([`crate::hooks::base_env`] — the same keys a local hook gets, with the
/// server's paths) plus the caller's injected pairs on top, so a declared `env`
/// key overrides a base one. The one owner of the remote side of that contract:
/// both the lifecycle dispatch and the `status` probe build their env here, so a
/// new `WS_*` key can't reach one and miss the other.
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

/// Borrow owned env pairs as the `(&str, &str)` slice the script builders take.
fn as_pairs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

/// Run a lifecycle command on the server: `cd` into the synced worktree, export
/// the `WS_*` contract plus the injected env, and `sh -c <command>`. Inherits
/// stdio (the server's output streams straight through) and mirrors the exit
/// status. The caller flushes the worktree's mutagen sync first so the server
/// sees the latest files.
///
/// The base `WS_*` here is [`crate::hooks::base_env`]'s local contract verbatim,
/// but the paths are the server's: `WS_WORKTREE` is the synced dir,
/// `WS_WORKSPACE_ROOT` the synced-worktrees base (there's no `.ws.json` workspace
/// root on the server).
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
    // Give the hook the PATH the user would have in their own terminal here, so a
    // `kind: host` repo's toolchain (nvm/asdf/pyenv/mise) resolves. Best-effort and
    // cached; an unavailable probe just leaves the default non-interactive PATH.
    // Appended after `extra_env` so a repo that declares its own PATH still wins
    // (`harvested_path` yields nothing when one is declared).
    env.extend(harvested_path(Some(connection), extra_env));
    let cmd = ["sh".to_string(), "-c".to_string(), command.to_string()];
    let script = remote_script(&dir, &as_pairs(&env), &cmd);
    // Pipe the SSH stdio and stream it through the same `hook_line` path a local
    // hook uses, instead of letting the ssh child inherit the terminal — otherwise
    // the server's output writes straight over the step spinner (the interleaving
    // wart). Now a remote hook lands with the same `│ [repo-event]` gutter and
    // suspends the spinner per line, identical to local.
    let child = ssh_command(connection, &script, false)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn ssh to {connection}"))?;
    crate::hooks::stream_and_wait(child, &format!("{repo}-{event}"))
}

/// Run a repo's read-only `status` command on the server and return its first
/// non-empty stdout line — the remote analogue of `hooks::capture_action`. Runs
/// in the synced worktree with the `WS_*` contract + injected env, bounded by a
/// server-side `timeout` so a wedged probe can't stall `ws list`. `None` on any
/// failure (unreachable, non-zero, empty). `base` is the absolute server worktree
/// base (home-resolved by the caller).
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
    // Deliberately no harvested login PATH, unlike `run_event`: this probe is
    // read-only, runs once per repo behind `ws list`, and the harvest costs an
    // extra SSH round trip on a cold cache.
    let env = event_env(base, &dir, task, repo, "status", extra_env);
    let script = status_script(&dir, &as_pairs(&env), command, timeout_secs);

    let out = ssh_capture(connection, &script)?;
    crate::ui::first_nonempty_line(&out).map(String::from)
}

/// Assemble the `ssh` command for `connection` running `script`. Passes `-t`
/// (force a remote pty) when `want_tty` — the caller decides based on whether
/// our stdio is interactive; see `exec`. Interactive work (tinker, prompts,
/// colors) gets a pty; piped/redirected/test invocations don't, keeping output
/// clean.
///
/// Not `batch` (see `ssh_base`): these calls may need to prompt for SSH auth.
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
        // Real `ssh -G` shape: lowercased keyword, space, value. The `hostname`
        // line is post-`~/.ssh/config`, so an alias arrives already rewritten.
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
    fn site_paths_are_the_execution_sites_own() {
        let root = Path::new("/home/me/myapp");
        let wt = Path::new("/home/me/myapp/.worktrees/feat-foo/api");
        // Local: exactly the host paths the caller already holds.
        assert_eq!(Site::Local.root_at(None, root), "/home/me/myapp");
        assert_eq!(
            Site::Local.worktree_at(None, root, wt, "feat-foo", "api"),
            wt.to_string_lossy()
        );
        // Remote: the server's base (home-resolved, so a `cd`'d script can't
        // double-resolve it) and the synced worktree under it — never the client's
        // paths, which don't exist there.
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
        // The contract has one owner (`hooks::base_env`) so a new `WS_*` key can't
        // reach a local hook and miss the remote one; the injected pairs land last
        // so a declared `env` key overrides a base one.
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
        // Same env-word rule as the dispatch path: a key that isn't a shell name
        // goes through `env`, or the server shell would run it as a command and
        // `ws list` would show a dash forever.
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
        // The realistic case: an interactive rc prints a prompt-framework banner
        // and `nvm use` chatter before our printf ever runs. Anything ahead of the
        // sentinel is the user's shell talking, not the answer.
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
        // configurations; without the marker there is no answer to trust, and
        // guessing at the last line could hand a hook a garbage PATH.
        assert_eq!(parse_probe("/usr/bin:/bin\n"), None);
        assert_eq!(parse_probe(""), None);
        // Sentinel present but nothing after it (rc killed mid-write).
        assert_eq!(parse_probe(&format!("{PROBE_SENTINEL}\n")), None);
        assert_eq!(parse_probe(&format!("{PROBE_SENTINEL}\n   \n")), None);
    }
}
