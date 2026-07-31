//! Host-process supervision for `kind: host` `up` scripts — a `ws`-owned detached
//! process tracked by a PID file, working the same on every platform.
//!
//! A host repo's `up` is a long-running dev server (`vite`, `cargo watch -x run`,
//! `php artisan serve`). Running it via `sh -c` and waiting would block `ws up`
//! forever, so instead `ws` starts it **detached** — `nohup … &`, stdio to a log,
//! its PID recorded — and returns. `down` kills it; `ws list` checks liveness with
//! `kill -0`.
//!
//! Everything runs through the [`Site`] abstraction, so a **remote** workspace
//! supervises the process **on the server** over SSH, right beside its containers —
//! the whole stack stays where the workspace points, no host/remote split. And
//! because it needs no `systemd` or `launchd`, macOS (podman-in-a-VM, no systemd)
//! is first-class: it's the identical code path as Linux.
//!
//! **Identity, not just liveness.** The pid file records the pid *and* the process
//! start time (`ps -o lstart`). Liveness re-checks both, so a stale pid file whose
//! number was recycled by an unrelated process after a reboot reads as *not* ours —
//! `up` won't wrongly skip, and `down` won't signal a stranger. No process-group
//! kill: a detached process under a non-interactive shell has no controllable job
//! control (`set -m` needs a tty) and macOS has no `setsid` binary, so a portable
//! group kill isn't available. We signal the launched process directly; a dev
//! server that forks its own long-lived grandchildren must reap them itself (most —
//! `vite`, `bun` — tear down with their parent).
//!
//! Trade vs a service manager: no crash-restart, and the process dies on
//! logout/reboot (you `ws up` again). Fine for a dev server you start deliberately.

use anyhow::{bail, Context, Result};
use std::process::Stdio;

use crate::remote::{shell_quote, Site};
use crate::workspace;

/// The per-worktree process name — the PID/log-file basename,
/// `ws-<sanitized-branch>-<repo>`. Mirrors the mutagen session naming so the whole
/// per-worktree namespace is consistent and greppable.
pub fn proc_name(branch: &str, repo: &str) -> String {
    format!("ws-{}-{}", workspace::sanitize_branch(branch), repo)
}

/// Emitted by the launch script when the process is gone half a second later, and
/// followed by the tail of its log. Distinct enough that a dev server printing it
/// by accident isn't a realistic concern.
const DIED_MARKER: &str = "__WS_HOST_DIED__";

/// The run dir and pid/log paths for `name`, as shell expressions evaluated at the
/// site (`$HOME` resolves to the server's home for a remote workspace). The basename
/// is `shell_quote`d: `name` derives from the branch and repo, which can carry
/// hostile characters from a cloned `.ws.json`, and a bare `{name}` inside a
/// double-quoted path would still expand `$(…)`.
fn paths(name: &str) -> (String, String, String) {
    let pid = format!("\"$HOME/.ws-run/\"{}", shell_quote(&format!("{name}.pid")));
    let log = format!("\"$HOME/.ws-run/\"{}", shell_quote(&format!("{name}.log")));
    ("\"$HOME/.ws-run\"".to_string(), pid, log)
}

/// A shell snippet that reads the pid file into `$p` (pid) and `$want` (identity),
/// leaving `$p` empty when the file is missing/blank. Shared by `is_active`/`down`.
fn read_pidfile(pid: &str) -> String {
    format!("p=; want=; {{ IFS= read -r p; IFS= read -r want; }} < {pid} 2>/dev/null")
}

/// Start `cmd` as a detached process named `name`, in `worktree`, with `env`.
/// Idempotent: if it's already running, leave it (a watcher reloads on file
/// change — no need to bounce a healthy server) and return `Ok(false)`; `Ok(true)`
/// on a fresh launch. A changed `cmd`/`env` only takes effect after `ws down`.
pub fn up(site: &Site, name: &str, worktree: &str, cmd: &str, env: &[(String, String)]) -> Result<bool> {
    if is_active(site, name) {
        return Ok(false);
    }
    let (dir, pid, log) = paths(name);
    // A `kind: host` repo's toolchain comes from the user's shell environment, and a
    // detached process on the server inherits the non-interactive SSH env — where a
    // version-manager `node` doesn't exist. So `#!/usr/bin/env node` shebangs (vite,
    // next, every JS dev server) die at launch with `env: 'node': No such file`, in
    // the log, after `ws` has already reported the step green. Same harvest the hook
    // dispatch uses; this path builds its own command and would otherwise miss it —
    // the same bypass that once cost host scripts their base `WS_*` contract.
    //
    // Emitted first so a repo declaring its own PATH in `env` still wins (`env`
    // applies assignments left to right).
    let harvested = match site {
        Site::Remote { connection, .. } => crate::remote::login_path(connection),
        Site::Local => None,
    };
    // Env as an `env K=V …` prefix. The whole `K=V` token is `shell_quote`d (not
    // just the value): a declared env key from `.ws.json` is untrusted, and quoting
    // the token keeps `env` seeing exactly one argument, closing key injection.
    let mut env_prefix = String::new();
    if let Some(p) = harvested.filter(|_| !env.iter().any(|(k, _)| k == "PATH")) {
        env_prefix.push_str(&format!("{} ", shell_quote(&format!("PATH={p}"))));
    }
    env_prefix
        .extend(env.iter().map(|(k, v)| format!("{} ", shell_quote(&format!("{k}={v}")))));
    // `nohup … &` detaches (survives `ws` exiting and the terminal / SSH session
    // closing); stdio to the log so a remote `ssh` returns at once instead of
    // holding the channel open. `$!` is the launched process — `nohup`/`env`/`sh`
    // all `exec`, preserving the pid. We record pid + start time as the identity.
    // Settle, then confirm it's actually still there. Launching always "succeeds" —
    // `nohup … &` returns a pid whether or not the command can run at all — so
    // without this a process that dies on the spot is reported as started, and the
    // only evidence is a log file the user has no reason to look at. Every real
    // failure of this kind is instant: an unresolvable `#!/usr/bin/env node`
    // shebang, a dev server whose first-run setup needs a tty it doesn't have.
    //
    // `kill -0` alone is not enough: the launching shell is the parent, so a process
    // that has already exited is a zombie until reaped, and signalling a zombie
    // succeeds. The process state answers properly — `Z` is dead, absent is dead.
    // Same `ps -o` idiom the identity check already relies on, so it works at both
    // sites.
    let script = format!(
        "mkdir -p {dir} || exit 1\n\
         cd {wt} || exit 1\n\
         nohup env {env_prefix}sh -c {cmd} </dev/null >{log} 2>&1 &\n\
         p=$!\n\
         printf '%s\\n%s\\n' \"$p\" \"$(ps -o lstart= -p \"$p\" 2>/dev/null | tr -d ' ')\" > {pid}\n\
         sleep 0.5\n\
         st=$(ps -o stat= -p \"$p\" 2>/dev/null | tr -d ' ')\n\
         case \"${{st:-gone}}\" in Z*|gone) echo {marker}; tail -n 20 {log} ;; esac",
        wt = shell_quote(worktree),
        cmd = shell_quote(cmd),
        marker = DIED_MARKER,
    );
    let out = site
        .shell(&script)
        .stdin(Stdio::null())
        .output()
        .with_context(|| "spawn host process")?;
    if !out.status.success() {
        bail!("host process: {}", first_line(&String::from_utf8_lossy(&out.stderr)));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(tail) = stdout.split_once(DIED_MARKER).map(|(_, t)| t) {
        // Put the log where the user is already looking. The reason a dev server
        // refused to start is the whole content of this failure; making them go
        // find `~/.ws-run/<name>.log` is what turned this into a long hunt.
        for line in tail.lines().map(str::trim).filter(|l| !l.is_empty()) {
            crate::ui::hook_line(name, line, true);
        }
        bail!("exited immediately — see ~/.ws-run/{name}.log");
    }
    Ok(true)
}

/// Stop a host process (and drop its pid file). A process that was never started,
/// already gone, or whose pid was recycled (identity mismatch) is a safe no-op.
/// Sends `TERM`, waits briefly, then `KILL`s — so on return the process is dead and
/// a caller (teardown) won't remove the worktree from under a live process.
pub fn down(site: &Site, name: &str) -> Result<()> {
    let (_, pid, _) = paths(name);
    let read = read_pidfile(&pid);
    // Only signal when the pid is alive AND its start time matches what we recorded
    // — never a recycled pid belonging to an unrelated process. `KILL` after a short
    // `TERM` grace guarantees the process is gone before we drop the pid file.
    let script = format!(
        "{read}\n\
         if [ -n \"$p\" ] && kill -0 \"$p\" 2>/dev/null; then\n\
         \x20 got=$(ps -o lstart= -p \"$p\" 2>/dev/null | tr -d ' ')\n\
         \x20 if [ \"$got\" = \"$want\" ]; then\n\
         \x20   kill -TERM \"$p\" 2>/dev/null\n\
         \x20   for _ in 1 2 3 4 5 6; do kill -0 \"$p\" 2>/dev/null || break; sleep 0.5; done\n\
         \x20   kill -KILL \"$p\" 2>/dev/null\n\
         \x20 fi\n\
         fi\n\
         rm -f {pid}",
    );
    let status = site
        .shell(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| "stop host process")?;
    // The script itself always exits 0; a non-zero status is the transport failing
    // (e.g. ssh couldn't reach the server) — surface it so teardown reports it.
    if !status.success() {
        bail!("stop host process: could not reach the site");
    }
    Ok(())
}

/// Whether the host process is currently running — its pid file exists, the pid is
/// alive (`kill -0`), and its start time matches the recorded identity (so a
/// recycled pid isn't mistaken for ours). Over a flaky remote link the SSH call
/// fails and this returns `false` (same best-effort posture as the rest of teardown).
pub fn is_active(site: &Site, name: &str) -> bool {
    let (_, pid, _) = paths(name);
    let read = read_pidfile(&pid);
    let script = format!(
        "{read}\n\
         [ -n \"$p\" ] || exit 1\n\
         kill -0 \"$p\" 2>/dev/null || exit 1\n\
         got=$(ps -o lstart= -p \"$p\" 2>/dev/null | tr -d ' ')\n\
         [ \"$got\" = \"$want\" ]",
    );
    site.shell(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_name_sanitizes_branch() {
        assert_eq!(proc_name("feat/foo", "api"), "ws-feat-foo-api");
        assert_eq!(proc_name("plain", "web"), "ws-plain-web");
    }

    #[test]
    fn paths_quote_a_hostile_name() {
        // A name carrying a command substitution must be single-quoted so it can't
        // execute when the path expression is evaluated by the shell.
        let (_, pid, _) = paths("ws-feat-x$(touch owned)-api");
        assert!(pid.contains("'ws-feat-x$(touch owned)-api.pid'"), "name not quoted: {pid}");
        assert!(pid.starts_with("\"$HOME/.ws-run/\""), "home must still expand: {pid}");
    }
}
