//! Host-process supervision for `kind: host` `up` scripts — a `ws`-owned detached
//! process tracked by a PID file, working the same on every platform.
//!
//! A host repo's `up` is a long-running dev server (`vite`, `cargo watch -x run`,
//! `php artisan serve`). Running it via `sh -c` and waiting would block `ws up`
//! forever, so instead `ws` starts it **detached** — `nohup … &`, stdio to a log,
//! its PID recorded — and returns. `down` kills it; [`health`] reports what it is
//! actually doing, which is what `ws list` shows and `ws up` checks after launch.
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
//! logout/reboot (you `ws up` again). Fine for a dev server you start deliberately
//! — and [`health`] at least makes a crash *visible*, which is the half of
//! supervision that was actually missing.

use anyhow::{bail, Context, Result};
use std::process::Stdio;

use crate::remote::{shell_quote, Site};
use crate::workspace;

/// The per-worktree process name — the PID/log-file basename,
/// `ws-<sanitized-branch>-<repo>`. The same [`workspace::ws_name`] the mutagen
/// session uses, so the whole per-worktree namespace is consistent and greppable.
pub fn proc_name(branch: &str, repo: &str) -> String {
    workspace::ws_name(branch, repo)
}

/// Emitted by the launch script when the process is gone half a second later, and
/// followed by the tail of its log. Distinct enough that a dev server printing it
/// by accident isn't a realistic concern.
const DIED_MARKER: &str = "__WS_HOST_DIED__";

/// Emitted by `down` when the recorded pid is alive but its identity doesn't match
/// — something is running under this name that ws can't confirm is its own, so it
/// is left alone and said out loud rather than quietly forgotten.
const STRANGER_MARKER: &str = "__WS_HOST_STRANGER__";

/// The run dir, as a shell expression evaluated at the site (`$HOME` resolves to the
/// server's home for a remote workspace).
const RUN_DIR: &str = "\"$HOME/.ws-run\"";

/// The `name.ext` path under the run dir, as a shell expression evaluated at the
/// site (`$HOME` resolves to the server's home for a remote workspace). The basename
/// is `shell_quote`d: `name` derives from the branch and repo, which can carry
/// hostile characters from a cloned `.ws.json`, and a bare `{name}` inside a
/// double-quoted path would still expand `$(…)`.
fn run_path(name: &str, ext: &str) -> String {
    format!("\"$HOME/.ws-run/\"{}", shell_quote(&format!("{name}.{ext}")))
}

/// The same `name.ext` file as [`run_path`], spelled the way a user would type it
/// (`~/.ws-run/<name>.<ext>`) for a message that sends them to look at it. Ops
/// point at these files too, so the layout stays owned here — a moved run dir
/// would otherwise leave several messages naming a directory that's gone.
fn run_hint(name: &str, ext: &str) -> String {
    format!("~/.ws-run/{name}.{ext}")
}

/// Where a host process's output lands, for a message telling the user to look.
pub fn log_hint(name: &str) -> String {
    run_hint(name, "log")
}

/// A shell snippet that reads the pid file into `$p` (pid) and `$want` (identity),
/// leaving `$p` empty when the file is missing/blank. Shared by `is_active`/`down`.
fn read_pidfile(pid: &str) -> String {
    format!("p=; want=; {{ IFS= read -r p; IFS= read -r want; }} < {pid} 2>/dev/null")
}

/// A shell snippet defining `ws_tree`, which prints `$1` followed by every
/// descendant pid.
///
/// The recorded pid is not reliably the process that matters. `nohup env … sh -c
/// <cmd>` only ends up *being* the dev server if every layer `exec`s through, and
/// whether that happens depends on the shape of the command: an inline
/// `… && exec vite` replaces the shell, while a path to a script leaves the
/// wrapper alive as a parent. Signalling the recorded pid alone then kills the
/// wrapper and reparents the real server to init, where it keeps holding its
/// port — so each `ws down`/`ws rm` leaked another dev server and ports crept
/// upward as allocation probed past the ones the strays still held.
///
/// A process-*group* kill would be the obvious answer and isn't available: job
/// control needs a tty (`set -m`) and macOS ships no `setsid`. Walking `ps` is
/// portable to both sites and costs one extra invocation.
fn tree_fn() -> &'static str {
    "ws_tree() {\n\
     \x20 all=$1; frontier=$1\n\
     \x20 while [ -n \"$frontier\" ]; do\n\
     \x20   kids=$(ps -eo pid=,ppid= 2>/dev/null | awk -v w=\"$frontier\" \
     'BEGIN{n=split(w,a,\" \");for(i=1;i<=n;i++)want[a[i]]=1} want[$2]{print $1}')\n\
     \x20   [ -n \"$kids\" ] || break\n\
     \x20   all=\"$all $kids\"; frontier=$kids\n\
     \x20 done\n\
     \x20 echo $all\n\
     }"
}

/// Start `cmd` as a detached process named `name`, in `worktree`, with `env`.
/// Idempotent: if it's already running, leave it (a watcher reloads on file
/// change — no need to bounce a healthy server) and return `Ok(false)`; `Ok(true)`
/// on a fresh launch. A changed `cmd`/`env` only takes effect after `ws down`.
pub fn up(site: &Site, name: &str, worktree: &str, cmd: &str, env: &[(String, String)]) -> Result<bool> {
    if is_active(site, name) {
        return Ok(false);
    }
    let (dir, pid, log) = (RUN_DIR, run_path(name, "pid"), run_path(name, "log"));
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
        "{tree}\n\
         mkdir -p {dir} || exit 1\n\
         cd {wt} || exit 1\n\
         nohup env {env_prefix}sh -c {cmd} </dev/null >{log} 2>&1 &\n\
         p=$!\n\
         printf '%s\\n%s\\n' \"$p\" \"$(ps -o lstart= -p \"$p\" 2>/dev/null | tr -d ' ')\" > {pid}\n\
         sleep 0.5\n\
         alive=\n\
         for v in $(ws_tree \"$p\"); do\n\
         \x20 st=$(ps -o stat= -p \"$v\" 2>/dev/null | tr -d ' ')\n\
         \x20 case \"${{st:-gone}}\" in Z*|gone) ;; *) alive=1 ;; esac\n\
         done\n\
         [ -n \"$alive\" ] || {{ echo {marker}; tail -n 20 {log}; }}",
        tree = tree_fn(),
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
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("host process: {}", crate::ui::first_nonempty_line(&err).unwrap_or("failed"));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(tail) = stdout.split_once(DIED_MARKER).map(|(_, t)| t) {
        // Put the log where the user is already looking. The reason a dev server
        // refused to start is the whole content of this failure; making them go
        // find `~/.ws-run/<name>.log` is what turned this into a long hunt.
        for line in tail.lines().map(str::trim).filter(|l| !l.is_empty()) {
            crate::ui::hook_line(name, line, true);
        }
        bail!("exited immediately — see {}", log_hint(name));
    }
    Ok(true)
}

/// The `down` script for the pid file at shell expression `pid`. Split out from
/// [`down`] so the shell itself can be exercised — the bugs this snippet has had
/// all live in the shell text, not in the Rust around it.
fn down_script(pid: &str) -> String {
    // Only signal when the pid is alive AND its start time matches what we recorded
    // — never a recycled pid belonging to an unrelated process. `KILL` after a short
    // `TERM` grace guarantees the process is gone before we drop the pid file.
    // The whole tree, not just the recorded pid — see `tree_fn`. Descendants are
    // collected *before* the first signal: killing the wrapper first would reparent
    // the dev server to init and lose the link to it.
    //
    // The tree is carried in the positional parameters and always expanded as
    // `"$@"`, never as an unquoted variable. On a remote workspace this script is
    // handed to the *login* shell (see `Site::shell`), and zsh — the macOS default
    // and a common Linux login shell — word-splits unquoted command substitution
    // but *not* unquoted parameter expansion. `kill -TERM $victims` there passed
    // the single argument "1234 5678" ("illegal pid"), killed nothing, and fell
    // through to `rm -f` on the pid file: `ws down` reported success while leaking
    // the whole tree and discarding the only record of it.
    format!(
        "{tree}\n\
         {read}\n\
         if [ -n \"$p\" ] && kill -0 \"$p\" 2>/dev/null; then\n\
         \x20 got=$(ps -o lstart= -p \"$p\" 2>/dev/null | tr -d ' ')\n\
         \x20 if [ \"$got\" = \"$want\" ]; then\n\
         \x20   set -- $(ws_tree \"$p\")\n\
         \x20   kill -TERM \"$@\" 2>/dev/null\n\
         \x20   for _ in 1 2 3 4 5 6; do\n\
         \x20     alive=; for v in \"$@\"; do kill -0 \"$v\" 2>/dev/null && alive=1; done\n\
         \x20     [ -n \"$alive\" ] || break\n\
         \x20     sleep 0.5\n\
         \x20   done\n\
         \x20   kill -KILL \"$@\" 2>/dev/null\n\
         \x20   rm -f {pid}\n\
         \x20 else\n\
         \x20   echo {stranger}\n\
         \x20 fi\n\
         else\n\
         \x20 rm -f {pid}\n\
         fi",
        tree = tree_fn(),
        read = read_pidfile(pid),
        stranger = STRANGER_MARKER,
    )
}

/// Stop a host process and drop its pid file. A process that was never started or
/// has already gone is a safe no-op. Sends `TERM`, waits briefly, then `KILL`s — so
/// on return the process is dead and a caller (teardown) won't remove the worktree
/// from under a live process.
///
/// A recorded pid that is alive but whose identity does not match is left running
/// **and reported**. It used to be skipped silently while the pid file was removed
/// anyway — discarding the only record of a live process ws had just declined to
/// kill. The next `ws up` then found no pid file, concluded nothing was running,
/// and started a second dev server beside the first; repeat, and the ports creep
/// upward as allocation probes past the ones the strays still hold. Refusing to
/// signal a stranger is right; forgetting it exists is what turned that caution
/// into a leak.
pub fn down(site: &Site, name: &str) -> Result<()> {
    let pid = run_path(name, "pid");
    let script = down_script(&pid);
    let out = site
        .shell(&script)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| "stop host process")?;
    let status = out.status;
    if String::from_utf8_lossy(&out.stdout).contains(STRANGER_MARKER) {
        crate::ui::note(&format!(
            "{name}: a process is recorded but its identity doesn't match — left running. \
             Check `{pid_file}` and stop it by hand if it's stale.",
            pid_file = run_hint(name, "pid"),
        ));
    }
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
    let pid = run_path(name, "pid");
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

/// What a supervised host process is actually *doing*, as opposed to what `ws`
/// reported when it launched it.
///
/// [`up`], [`down`] and [`is_active`] all answer the same question: is a process
/// with our recorded identity alive. That is the *action* ws took, not the
/// *effect* it implied. A dev server can be perfectly alive while serving a port
/// ws never allocated (vite silently increments past a port it finds busy), and
/// the port ws did allocate can be held by an orphan from an earlier run. Both
/// read as healthy to a pid check, and both surface somewhere unrelated — an
/// empty port, a 502, a stale server answering — which is what makes them
/// expensive to chase.
///
/// So this asks the harder question: is the port ws allocated being listened on
/// *by this process tree*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// No pid file — never started, or stopped cleanly by `ws down`.
    NotStarted,
    /// A pid was recorded but nothing matching it is running: it exited without
    /// a `ws down`. The state a user has no way to see today.
    Crashed,
    /// Alive, and its own process tree holds every port ws allocated.
    Serving(Vec<u16>),
    /// Alive, but nothing is listening on these allocated ports — still starting
    /// up, or bound somewhere ws didn't ask for.
    Silent(Vec<u16>),
    /// Alive, but these allocated ports are held by a process *outside* its tree
    /// — an orphan from an earlier run, so the real server has moved elsewhere.
    Squatted(Vec<u16>),
    /// Alive; the ports couldn't be attributed (no `ss` or `lsof` at the site, or
    /// nothing was allocated to check). Liveness only — what ws knew before.
    Running,
}

impl Health {
    /// Whether this is the state a caller should be happy with. Both variants
    /// mean "alive with nothing to report"; the rest all warrant saying something.
    pub fn is_ok(&self) -> bool {
        matches!(self, Health::Serving(_) | Health::Running)
    }

    /// A short run-state phrase for `ws list`'s status column and `ws up`'s step.
    pub fn label(&self) -> String {
        let list = |ports: &[u16]| {
            ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(", ")
        };
        match self {
            Health::NotStarted => "not started".to_string(),
            Health::Crashed => "crashed".to_string(),
            Health::Serving(p) => format!("running {}", list(p)),
            Health::Silent(p) => format!("running, nothing on {}", list(p)),
            Health::Squatted(p) => format!("running, {} held by another process", list(p)),
            Health::Running => "running".to_string(),
        }
    }
}

/// A shell snippet defining `ws_owner`, which prints who is listening on port
/// `$1`: `ours` (a pid in the tree `$2`), `other`, `none`, or `unknown`.
///
/// Attribution, not just occupancy, is the whole point: "something is listening
/// on :20001" is true both when the dev server bound it and when the orphan that
/// pushed the dev server onto another port did. Only the pid distinguishes them.
///
/// `ss` (Linux) and `lsof` (macOS) are the two portable ways to get that pid, and
/// both report it for sockets owned by the querying user — which the supervised
/// process always is. Neither present ⇒ `unknown`, so the caller degrades to plain
/// liveness rather than inventing a verdict. A socket that *is* listening but
/// whose owner we can't see reads as `other`: it isn't ours as far as we can tell,
/// which is the safe direction — it says "look at this", not "all good".
///
/// Neither pid list is iterated as a bare `$var`: the listeners are re-split through
/// a command substitution and the tree is matched by substring. On a remote this
/// runs under the server's *login* shell (see `Site::shell`), and zsh doesn't
/// word-split unquoted parameter expansions — `for t in $2` saw the whole tree as
/// one word, so a healthy multi-pid host process never matched and every port came
/// back `other` ("running, :NNNNN held by another process").
fn owner_fn() -> &'static str {
    "ws_owner() {\n\
     \x20 if command -v ss >/dev/null 2>&1; then\n\
     \x20   found=$(ss -Hltnp 2>/dev/null | awk -v p=\"$1\" '$4 ~ (\"[.:]\" p \"$\")')\n\
     \x20   [ -n \"$found\" ] || { echo none; return; }\n\
     \x20   pids=$(echo \"$found\" | tr ',' '\\n' | sed -n 's/^pid=\\([0-9][0-9]*\\).*/\\1/p')\n\
     \x20 elif command -v lsof >/dev/null 2>&1; then\n\
     \x20   pids=$(lsof -nP -iTCP:\"$1\" -sTCP:LISTEN -t 2>/dev/null)\n\
     \x20   [ -n \"$pids\" ] || { echo none; return; }\n\
     \x20 else\n\
     \x20   echo unknown; return\n\
     \x20 fi\n\
     \x20 [ -n \"$pids\" ] || { echo other; return; }\n\
     \x20 for q in $(echo $pids); do\n\
     \x20   case \" $2 \" in *\" $q \"*) echo ours; return;; esac\n\
     \x20 done\n\
     \x20 echo other\n\
     }"
}

/// Probe what the host process named `name` is really doing, given the ports ws
/// allocated for it. `None` when the site couldn't be reached or answered
/// something unrecognisable — an unknown state, never a green one.
///
/// One round trip: liveness, identity, and every port verdict come back together,
/// which matters because a remote workspace pays SSH for each one.
pub fn health(site: &Site, name: &str, ports: &[u16]) -> Option<Health> {
    let pid = run_path(name, "pid");
    let read = read_pidfile(&pid);
    // The port list is `u16`s we formatted ourselves — no config value reaches
    // the script here. An empty list omits the loop entirely (`for x in ; do` is
    // a syntax error) and the verdict falls back to plain liveness.
    let probe = if ports.is_empty() {
        String::new()
    } else {
        format!(
            "t=$(ws_tree \"$p\")\n\
             for port in {}; do echo \"port $port $(ws_owner \"$port\" \"$t\")\"; done",
            ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ")
        )
    };
    let script = format!(
        "{tree}\n\
         {owner}\n\
         {read}\n\
         [ -n \"$p\" ] || {{ echo nopid; exit 0; }}\n\
         kill -0 \"$p\" 2>/dev/null || {{ echo dead; exit 0; }}\n\
         got=$(ps -o lstart= -p \"$p\" 2>/dev/null | tr -d ' ')\n\
         [ \"$got\" = \"$want\" ] || {{ echo dead; exit 0; }}\n\
         echo alive\n\
         {probe}",
        tree = tree_fn(),
        owner = owner_fn(),
    );
    let out = site
        .shell(&script)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_health(&String::from_utf8_lossy(&out.stdout))
}

/// Like [`health`], but gives a just-launched process a moment to bind before
/// judging it. Returns as soon as it's serving.
///
/// A dev server does not listen the instant it is spawned — vite takes a beat,
/// `cargo watch` compiles first — so a single immediate probe would report half
/// the healthy launches as silent. Waiting a little and *still* reporting the
/// unconfirmed case (rather than blocking until it binds) keeps a slow starter
/// from stalling `ws up` behind a compile.
pub fn health_settled(
    site: &Site,
    name: &str,
    ports: &[u16],
    wait: std::time::Duration,
) -> Option<Health> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let got = health(site, name, ports);
        // Anything but "alive, not listening yet" is already final: a crash won't
        // un-crash, and a squatter won't release the port by itself.
        if !matches!(got, Some(Health::Silent(_))) || std::time::Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

/// Turn the probe script's output into a verdict. `None` for output that doesn't
/// parse — the caller reports "unknown", never a default.
fn parse_health(out: &str) -> Option<Health> {
    let (mut alive, mut unattributed) = (false, false);
    let (mut served, mut silent, mut squatted) = (Vec::new(), Vec::new(), Vec::new());
    for line in out.lines().map(str::trim) {
        match line {
            "nopid" => return Some(Health::NotStarted),
            "dead" => return Some(Health::Crashed),
            "alive" => alive = true,
            _ => {
                let Some(rest) = line.strip_prefix("port ") else { continue };
                let mut fields = rest.split_whitespace();
                let (Some(port), Some(verdict)) = (fields.next(), fields.next()) else { continue };
                let Ok(port) = port.parse::<u16>() else { continue };
                match verdict {
                    "ours" => served.push(port),
                    "none" => silent.push(port),
                    "other" => squatted.push(port),
                    _ => unattributed = true,
                }
            }
        }
    }
    if !alive {
        return None;
    }
    // Most actionable first: a port held by a stranger explains a server that
    // "started" but can't be reached, which a merely-silent port does not.
    if !squatted.is_empty() {
        return Some(Health::Squatted(squatted));
    }
    if !silent.is_empty() {
        return Some(Health::Silent(silent));
    }
    if unattributed || served.is_empty() {
        return Some(Health::Running);
    }
    Some(Health::Serving(served))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `script` through `sh` and return its trimmed stdout.
    fn sh_capture(script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run sh");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Whether `bin` is on PATH here.
    fn have(bin: &str) -> bool {
        !sh_capture(&format!("command -v {bin} 2>/dev/null")).is_empty()
    }

    /// A fresh scratch directory for a test's files.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ws-supervise-{tag}-{}-{uniq}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Whether `pid` is a *live* process — not merely a pid `kill -0` accepts. A
    /// process the test itself spawned is a zombie until reaped, and signalling a
    /// zombie succeeds, so liveness is read off the process state (the same `ps -o`
    /// idiom the launch script uses).
    fn pid_running(pid: u32) -> bool {
        let st = sh_capture(&format!("ps -o stat= -p {pid} 2>/dev/null | tr -d ' '"));
        !st.is_empty() && !st.starts_with('Z')
    }

    /// The first child of `pid`, waiting briefly for it to appear.
    fn child_of(pid: u32) -> Option<u32> {
        for _ in 0..40 {
            let out =
                sh_capture(&format!("ps -eo pid=,ppid= | awk -v w={pid} '$2==w{{print $1}}'"));
            if let Some(kid) = out.lines().next().and_then(|l| l.trim().parse().ok()) {
                return Some(kid);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }

    #[test]
    fn proc_name_sanitizes_branch() {
        assert_eq!(proc_name("feat/foo", "api"), "ws-feat-foo-api");
        assert_eq!(proc_name("plain", "web"), "ws-plain-web");
    }

    #[test]
    fn paths_quote_a_hostile_name() {
        // A name carrying a command substitution must be single-quoted so it can't
        // execute when the path expression is evaluated by the shell.
        let pid = run_path("ws-feat-x$(touch owned)-api", "pid");
        assert!(pid.contains("'ws-feat-x$(touch owned)-api.pid'"), "name not quoted: {pid}");
        assert!(pid.starts_with("\"$HOME/.ws-run/\""), "home must still expand: {pid}");
    }

    #[test]
    fn health_reads_every_state_the_probe_can_report() {
        assert_eq!(parse_health("nopid\n"), Some(Health::NotStarted));
        assert_eq!(parse_health("dead\n"), Some(Health::Crashed));
        assert_eq!(parse_health("alive\nport 20000 ours\n"), Some(Health::Serving(vec![20000])));
        assert_eq!(parse_health("alive\nport 20000 none\n"), Some(Health::Silent(vec![20000])));
        assert_eq!(parse_health("alive\nport 20000 other\n"), Some(Health::Squatted(vec![20000])));
        // Neither `ss` nor `lsof` at the site: liveness only, never a green verdict
        // invented from a probe that couldn't run.
        assert_eq!(parse_health("alive\nport 20000 unknown\n"), Some(Health::Running));
        // Alive with nothing allocated to check.
        assert_eq!(parse_health("alive\n"), Some(Health::Running));
    }

    #[test]
    fn health_is_unknown_rather_than_healthy_when_the_probe_says_nothing() {
        // A site that answered with output we don't recognise must not read as
        // running — the caller shows "unknown", which is the honest answer.
        assert_eq!(parse_health(""), None);
        assert_eq!(parse_health("bash: ss: command not found\n"), None);
    }

    #[test]
    fn a_squatted_port_outranks_a_merely_silent_one() {
        // Both are wrong, but a stranger holding the port explains *why* the server
        // is unreachable, so it's the finding worth surfacing.
        assert_eq!(
            parse_health("alive\nport 20000 none\nport 20001 other\n"),
            Some(Health::Squatted(vec![20001]))
        );
    }

    #[test]
    fn health_labels_say_what_to_do_about_it() {
        assert_eq!(Health::Serving(vec![20000]).label(), "running :20000");
        assert_eq!(Health::Silent(vec![20000]).label(), "running, nothing on :20000");
        assert_eq!(
            Health::Squatted(vec![20000]).label(),
            "running, :20000 held by another process"
        );
        assert!(Health::Serving(vec![20000]).is_ok());
        assert!(Health::Running.is_ok());
        assert!(!Health::Crashed.is_ok());
        assert!(!Health::Silent(vec![1]).is_ok());
    }

    #[test]
    fn the_probe_script_runs_and_reports_a_process_that_was_never_started() {
        // Exercises the real script (shell syntax included, which no amount of
        // parser testing would catch) against a name that has no pid file.
        assert_eq!(
            health(&Site::Local, "ws-no-such-worktree-probe-test", &[]),
            Some(Health::NotStarted)
        );
        // Same, with a port list — the branch that emits the `for` loop.
        assert_eq!(
            health(&Site::Local, "ws-no-such-worktree-probe-test", &[20000]),
            Some(Health::NotStarted)
        );
    }

    #[test]
    fn down_kills_the_whole_tree_under_a_shell_that_does_not_split_parameters() {
        // A remote workspace hands this script to the server's *login* shell
        // (`Site::shell`), which is zsh on macOS and on plenty of Linux dev boxes.
        // zsh word-splits unquoted command substitution but not unquoted parameter
        // expansion, so `kill -TERM $victims` passed one "1234 5678" argument,
        // signalled nothing, and fell through to `rm -f` on the pid file: `ws down`
        // reported success while leaking the tree and discarding the record of it.
        if !have("zsh") {
            eprintln!("skipping: no `zsh` to stand in for a non-POSIX login shell");
            return;
        }
        let dir = scratch_dir("down");
        let pid_path = dir.join("tree.pid");

        // Two pids: a wrapper `sh` that can't `exec` its child because something
        // follows it, plus the `sleep` itself — the shape `tree_fn` exists for.
        let mut wrapper = std::process::Command::new("sh")
            .args(["-c", "sleep 30; :"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a wrapper shell");
        let parent = wrapper.id();
        let child = child_of(parent).expect("the wrapper should have started a child");
        let want = sh_capture(&format!("ps -o lstart= -p {parent} 2>/dev/null | tr -d ' '"));
        std::fs::write(&pid_path, format!("{parent}\n{want}\n")).expect("write pid file");

        let script = down_script(&shell_quote(pid_path.to_str().unwrap()));
        let status = std::process::Command::new("zsh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .status()
            .expect("run the down script under zsh");

        let mut settled = false;
        for _ in 0..30 {
            if !pid_running(parent) && !pid_running(child) {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let (parent_running, child_running) = (pid_running(parent), pid_running(child));
        // Leave no stray behind, whatever the assertions below decide.
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(parent.to_string())
            .arg(child.to_string())
            .status();
        let _ = wrapper.wait();
        let pid_file_gone = !pid_path.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(status.success(), "the down script must exit 0, got {status}");
        assert!(
            !child_running,
            "the descendant must be killed, not left holding its port while the pid file is dropped"
        );
        assert!(!parent_running, "the recorded process must be killed");
        assert!(settled, "the whole tree must be gone once `down` returns");
        assert!(pid_file_gone, "a stopped process's pid file must be removed");
    }

    #[test]
    #[cfg(unix)]
    fn port_attribution_sees_individual_pids_in_every_shell() {
        // `ws_owner`'s two pid lists used to be iterated as bare `$pids`/`$2`, which
        // a login shell that doesn't split parameters (zsh) hands over as a single
        // word — so a healthy host process with more than one pid never matched and
        // `ws list` reported "held by another process" for its own server.
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("owner");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        // Stand-in `ss`: one listening socket held by two pids, as a forked dev
        // server reports. Deterministic, and it needs no real listener.
        let ss = bin.join("ss");
        std::fs::write(
            &ss,
            "#!/bin/sh\necho 'LISTEN 0 511 0.0.0.0:20000 0.0.0.0:* \
             users:((\"node\",pid=111,fd=3),(\"node\",pid=222,fd=4))'\n",
        )
        .expect("write fake ss");
        std::fs::set_permissions(&ss, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());

        let script = format!("{}\nws_owner \"$1\" \"$2\"", owner_fn());
        for shell in ["sh", "zsh", "bash", "dash"] {
            if !have(shell) {
                continue;
            }
            let verdict = |port: &str, tree: &str| {
                let out = std::process::Command::new(shell)
                    .args(["-c", script.as_str(), "ws-owner-test", port, tree])
                    .env("PATH", &path)
                    .stdin(Stdio::null())
                    .output()
                    .expect("run ws_owner");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            assert_eq!(
                verdict("20000", "999 222"),
                "ours",
                "{shell}: a listener pid inside the tree must attribute the port to us"
            );
            assert_eq!(
                verdict("20000", "999 333"),
                "other",
                "{shell}: a listener outside the tree is not ours"
            );
            assert_eq!(
                verdict("30000", "111"),
                "none",
                "{shell}: a port nobody listens on must read as free"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
