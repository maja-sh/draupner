//! Host-process supervision for `kind: host` `up` scripts: a detached process
//! tracked by a PID file, running at whichever [`Site`] the workspace points at.
//! See ARCHITECTURE.md for how it fits the rest of `ws`.
//!
//! The pid file records the pid *and* the process start time, and every check
//! re-reads both: a pid recycled by an unrelated process after a reboot has to
//! read as *not* ours, so `up` won't wrongly skip and `down` won't signal a
//! stranger.

use anyhow::{bail, Context, Result};
use std::process::Stdio;

use crate::remote::{shell_quote, Site};
use crate::workspace;

/// The PID/log-file basename. Shares [`workspace::ws_name`] with the mutagen
/// session so a worktree has one name across everything ws runs for it.
pub fn proc_name(branch: &str, repo: &str) -> String {
    workspace::ws_name(branch, repo)
}

/// Printed by the launch script when the process is already gone, ahead of the tail
/// of its log. Distinct enough that a dev server emitting it isn't a real concern.
const DIED_MARKER: &str = "__WS_HOST_DIED__";

/// Printed by `down` when the recorded pid is alive but its identity doesn't match
/// — something ws can't confirm is its own, so it is left alone and said out loud
/// rather than quietly forgotten.
const STRANGER_MARKER: &str = "__WS_HOST_STRANGER__";

/// The run dir, as a shell expression evaluated at the site (`$HOME` resolves to the
/// server's home for a remote workspace).
const RUN_DIR: &str = "\"$HOME/.ws-run\"";

/// The `name.ext` path under [`RUN_DIR`], as a shell expression evaluated at the
/// site. The basename is `shell_quote`d: `name` derives from the branch and repo,
/// which can carry hostile characters from a cloned `.ws.json`, and a bare `{name}`
/// inside a double-quoted path would still expand `$(…)`.
fn run_path(name: &str, ext: &str) -> String {
    format!("\"$HOME/.ws-run/\"{}", shell_quote(&format!("{name}.{ext}")))
}

fn run_hint(name: &str, ext: &str) -> String {
    format!("~/.ws-run/{name}.{ext}")
}

pub fn log_hint(name: &str) -> String {
    run_hint(name, "log")
}

/// A shell snippet reading the pid file into `$p` (pid) and `$want` (identity),
/// leaving `$p` empty when the file is missing or blank.
fn read_pidfile(pid: &str) -> String {
    format!("p=; want=; {{ IFS= read -r p; IFS= read -r want; }} < {pid} 2>/dev/null")
}

/// A shell snippet defining `ws_tree`, which prints `$1` followed by every
/// descendant pid.
///
/// The recorded pid is often a wrapper rather than the dev server: `nohup env … sh
/// -c <cmd>` only collapses onto the server when the command's shape lets every
/// layer `exec` through. Signalling it alone kills the wrapper and reparents the
/// real server to init, still holding its port. A process-*group* kill would be the
/// obvious answer and isn't portable: job control needs a tty (`set -m`) and macOS
/// ships no `setsid`.
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
/// Idempotent: an already-running process is left alone (a watcher reloads on file
/// change) and returns `Ok(false)`, which also means a changed `cmd`/`env` only
/// takes effect after `ws down`.
pub fn up(site: &Site, name: &str, worktree: &str, cmd: &str, env: &[(String, String)]) -> Result<bool> {
    if is_active(site, name) {
        return Ok(false);
    }
    let (dir, pid, log) = (RUN_DIR, run_path(name, "pid"), run_path(name, "log"));
    // A detached process on the server inherits the non-interactive SSH env, where a
    // version-manager `node` doesn't exist — so `#!/usr/bin/env node` shebangs (vite,
    // next, every JS dev server) die at launch, in a log the user has no reason to
    // read, after `ws` has already reported the step green. Harvested first so a repo
    // declaring its own PATH still wins (`env` applies assignments left to right, and
    // the harvest yields nothing when the caller declares one).
    let harvested = crate::remote::harvested_path(site.connection(), env);
    // The whole `K=V` token is `shell_quote`d, not just the value: a declared env key
    // from `.ws.json` is untrusted, and quoting the token keeps `env` seeing exactly
    // one argument, closing key injection.
    let mut env_prefix = String::new();
    for (k, v) in harvested.iter().chain(env) {
        env_prefix.push_str(&format!("{} ", shell_quote(&format!("{k}={v}"))));
    }
    // Stdio goes to the log so a remote `ssh` returns at once instead of holding the
    // channel open. Launching always "succeeds" — `nohup … &` yields a pid whether or
    // not the command can run at all — so without settling and re-checking, a process
    // that dies on the spot is reported as started. `kill -0` can't answer that: the
    // launching shell is the parent, so an already-exited process is a zombie until
    // reaped and signalling a zombie succeeds. The state distinguishes them — `Z` is
    // dead, absent is dead.
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
        // The reason a dev server refused to start is the whole content of this
        // failure, so it goes where the user is already looking.
        for line in tail.lines().map(str::trim).filter(|l| !l.is_empty()) {
            crate::ui::hook_line(name, line);
        }
        bail!("exited immediately — see {}", log_hint(name));
    }
    Ok(true)
}

/// The `down` script for the pid file at shell expression `pid`. Split out from
/// [`down`] so the shell itself can be exercised — every bug this snippet has had
/// lived in the shell text, not in the Rust around it.
fn down_script(pid: &str) -> String {
    // Descendants are collected *before* the first signal: killing the wrapper first
    // would reparent the dev server to init and lose the link to it. The tree then
    // rides in the positional parameters and is always expanded as `"$@"`, never as a
    // bare variable — on a remote this script is handed to the server's *login* shell
    // (see `Site::shell`), and zsh doesn't word-split unquoted parameter expansions, so
    // `kill -TERM $victims` passed the single argument "1234 5678", killed nothing, and
    // fell through to `rm -f` on the pid file.
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

/// Stop a host process and drop its pid file; never started or already gone is a
/// safe no-op. `TERM`, a short wait, then `KILL`, so on return the process is dead
/// and a caller (teardown) won't remove the worktree from under a live process.
///
/// A recorded pid that is alive but whose identity does not match is left running
/// **and reported**. Refusing to signal a stranger is right; dropping its pid file
/// too is what leaked — the next `ws up` found no record, concluded nothing was
/// running, and started a second dev server beside the first.
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

/// Whether the host process is alive and ours. [`health`] with no ports asks exactly
/// that, so there's one copy of the liveness+identity shell rather than a third
/// spelling. An unreachable site reads `false`, as elsewhere in teardown.
fn is_active(site: &Site, name: &str) -> bool {
    matches!(health(site, name, &[]), Some(Health::Running))
}

/// What a supervised host process is actually *doing*, as opposed to what `ws`
/// reported when it launched it. A dev server can be alive while serving a port ws
/// never allocated (vite silently increments past a busy one), and ws's port can be
/// held by an orphan from an earlier run — both read healthy to a pid check. So the
/// question asked here is whether the allocated port is held *by this process tree*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// No pid file — never started, or stopped cleanly by `ws down`.
    NotStarted,
    /// A pid was recorded but nothing matching it is running: it exited without a
    /// `ws down`.
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
    /// nothing was allocated to check). Liveness only.
    Running,
}

impl Health {
    /// `Running` counts as fine: liveness with no attribution available is not a
    /// finding, and the rest all warrant saying something.
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
/// Neither `ss` nor `lsof` present ⇒ `unknown`, so the caller degrades to plain
/// liveness rather than inventing a verdict; a listening socket whose owner we can't
/// see reads as `other`, the safe direction — "look at this", not "all good".
///
/// Neither pid list is iterated as a bare `$var`: the listeners are re-split through
/// a command substitution and the tree is matched by substring, because on a remote
/// this runs under the server's *login* shell (see `Site::shell`) and zsh doesn't
/// word-split unquoted parameter expansions — `for t in $2` saw a whole pid tree as
/// one word, so every port on a healthy multi-pid process came back `other`.
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
/// One round trip: liveness, identity and every port verdict come back together,
/// which matters because a remote workspace pays SSH for each one.
pub fn health(site: &Site, name: &str, ports: &[u16]) -> Option<Health> {
    let pid = run_path(name, "pid");
    let read = read_pidfile(&pid);
    // The ports are `u16`s we formatted ourselves — no config value reaches the
    // script here. An empty list must omit the loop entirely (`for x in ; do` is a
    // syntax error), leaving the verdict at plain liveness.
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
/// judging it. A dev server doesn't listen the instant it is spawned — vite takes a
/// beat, `cargo watch` compiles first — so an immediate probe calls half the healthy
/// launches silent. Returning the still-unconfirmed case rather than blocking until
/// it binds keeps a slow starter from stalling `ws up`.
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

    fn sh_capture(script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run sh");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn have(bin: &str) -> bool {
        !sh_capture(&format!("command -v {bin} 2>/dev/null")).is_empty()
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ws-supervise-{tag}-{}-{uniq}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Whether `pid` is a *live* process — not merely one `kill -0` accepts. A
    /// process the test itself spawned is a zombie until reaped, and signalling a
    /// zombie succeeds, so liveness is read off the process state.
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
        assert_eq!(parse_health("alive\n"), Some(Health::Running));
    }

    #[test]
    fn health_is_unknown_rather_than_healthy_when_the_probe_says_nothing() {
        assert_eq!(parse_health(""), None);
        assert_eq!(parse_health("bash: ss: command not found\n"), None);
    }

    #[test]
    fn a_squatted_port_outranks_a_merely_silent_one() {
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
        // Exercises the real script, shell syntax included, which no amount of parser
        // testing would catch.
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
    fn a_process_that_was_never_started_is_not_active() {
        assert!(!is_active(&Site::Local, "ws-no-such-worktree-active-test"));
    }

    #[test]
    fn down_kills_the_whole_tree_under_a_shell_that_does_not_split_parameters() {
        // zsh is the login shell a remote `down` script usually lands in, and it
        // word-splits unquoted command substitution but not parameter expansion — the
        // difference that once made `kill -TERM $victims` signal nothing while the pid
        // file was removed anyway.
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
        // A login shell that doesn't split parameters (zsh) hands a bare `$pids`/`$2`
        // over as a single word, so a host process with more than one pid never matched
        // and `ws list` reported "held by another process" for its own server.
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
