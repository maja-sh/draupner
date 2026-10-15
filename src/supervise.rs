//! Host-process supervision for `kind: host` `up` scripts. Identity is pid *and*
//! start time so a recycled pid reads as not ours. Process groups aren't portable
//! (macOS has no `setsid`), so long-lived grandchildren must self-reap.

use anyhow::{bail, Context, Result};
use std::process::Stdio;

use crate::remote::{shell_quote, Site};
use crate::workspace;

/// Per-worktree process name, matching the mutagen session naming.
pub fn proc_name(branch: &str, repo: &str) -> String {
    format!("drup-{}-{}", workspace::sanitize_branch(branch), repo)
}

/// Absolute log path at the *site* where the process runs — `~/.drup-run/<name>.log` expands against the local `$HOME` for a local site and the remote's captured `$HOME` for a remote one, so a caller printing the path never advertises a client path for a server-side log. `None` when the local `$HOME` isn't set.
pub fn log_path(site: &Site, name: &str) -> Option<std::path::PathBuf> {
    match site {
        Site::Local => {
            let home = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
            Some(std::path::PathBuf::from(home).join(".drup-run").join(format!("{name}.log")))
        }
        Site::Remote { .. } => site.home_path(&format!(".drup-run/{name}.log")).map(std::path::PathBuf::from),
    }
}

const DIED_MARKER: &str = "__WS_HOST_DIED__";
const STRANGER_MARKER: &str = "__WS_HOST_STRANGER__";

/// The basename is `shell_quote`d because `name` is untrusted and `$(…)` still
/// expands inside double quotes.
fn paths(name: &str) -> (String, String, String) {
    let pid = format!("\"$HOME/.drup-run/\"{}", shell_quote(&format!("{name}.pid")));
    let log = format!("\"$HOME/.drup-run/\"{}", shell_quote(&format!("{name}.log")));
    ("\"$HOME/.drup-run\"".to_string(), pid, log)
}

fn read_pidfile(pid: &str) -> String {
    format!("p=; want=; {{ IFS= read -r p; IFS= read -r want; }} < {pid} 2>/dev/null")
}

/// `ws_tree`: print `$1` and every descendant pid, so signalling the wrapper
/// doesn't orphan the dev server it forked (process-group kill isn't portable).
/// awk emits kids space-separated because bsd awk (macOS default) rejects
/// newlines in the next iteration's `-v w=`, truncating the BFS at depth 1.
/// The trailing `tr` flattens `echo`'s newline because zsh (remote login
/// shell) doesn't word-split unquoted expansions the way sh does.
fn tree_fn() -> &'static str {
    "ws_tree() {\n\
     \x20 all=$1; frontier=$1\n\
     \x20 while [ -n \"$frontier\" ]; do\n\
     \x20   kids=$(ps -eo pid=,ppid= 2>/dev/null | awk -v w=\"$frontier\" \
     'BEGIN{n=split(w,a,\" \");for(i=1;i<=n;i++)want[a[i]]=1} want[$2]{printf \"%s \", $1}')\n\
     \x20   [ -n \"$kids\" ] || break\n\
     \x20   all=\"$all $kids\"; frontier=$kids\n\
     \x20 done\n\
     \x20 echo $all | tr '\\n' ' '\n\
     }"
}

/// Start `cmd` as a detached process named `name`. Idempotent: an already-running
/// process returns `Ok(false)`. Changed `cmd`/`env` only takes effect after `drup down`.
pub fn up(site: &Site, name: &str, worktree: &str, cmd: &str, env: &[(String, String)]) -> Result<bool> {
    if is_active(site, name) {
        return Ok(false);
    }
    let (dir, pid, log) = paths(name);
    // Harvest a login PATH so `env node` shebangs resolve under non-interactive
    // SSH; emitted first so a repo's own PATH still wins.
    let harvested = match site {
        Site::Remote { connection, .. } => crate::remote::login_path(connection),
        Site::Local => None,
    };
    // Quote the whole `K=V` token, not just the value: the key is untrusted.
    let mut env_prefix = String::new();
    if let Some(p) = harvested.filter(|_| !env.iter().any(|(k, _)| k == "PATH")) {
        env_prefix.push_str(&format!("{} ", shell_quote(&format!("PATH={p}"))));
    }
    env_prefix
        .extend(env.iter().map(|(k, v)| format!("{} ", shell_quote(&format!("{k}={v}")))));
    // `nohup … &` returns a pid whether the command can run or not; re-check via
    // process state, since an unreaped zombie still signals `kill -0` ok.
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
        bail!("host process: {}", first_line(&String::from_utf8_lossy(&out.stderr)));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(tail) = stdout.split_once(DIED_MARKER).map(|(_, t)| t) {
        // Print the failing log inline instead of making the user hunt for it.
        for line in tail.lines().map(str::trim).filter(|l| !l.is_empty()) {
            crate::ui::hook_line(name, line, true);
        }
        bail!("exited immediately — see ~/.drup-run/{name}.log");
    }
    Ok(true)
}

/// Carries the tree as positional params (`set --` / `"$@"`) rather than an unquoted `$victims` — zsh, the macOS login shell, doesn't word-split unquoted parameter expansions, so the old form passed "1234 5678" as one arg ("illegal pid"), killed nothing, and dropped the pid file anyway.
fn down_script(pid: &str) -> String {
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

/// Stop a host process and drop its pid file. `TERM` then `KILL` — dead on return. A live pid whose identity doesn't match is left running *and reported*; silently forgetting would leak servers on the next `drup up`.
pub fn down(site: &Site, name: &str) -> Result<()> {
    let (_, pid, _) = paths(name);
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
             Check `~/.drup-run/{name}.pid` and stop it by hand if it's stale."
        ));
    }
    // The script always exits 0, so a non-zero status is transport failure.
    if !status.success() {
        bail!("stop host process: could not reach the site");
    }
    Ok(())
}

/// Whether the host process is running (pid alive and identity matches).
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

/// Liveness plus whether drup's allocated ports are held *by this process tree*
/// (vite may silently pick another port; an orphan may hold ours).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    NotStarted,
    Crashed,
    /// Alive; its tree holds every allocated port.
    Serving(Vec<u16>),
    /// Alive; nothing is listening on these ports.
    Silent(Vec<u16>),
    /// Alive; these ports are held outside its tree.
    Squatted(Vec<u16>),
    /// Alive; ports couldn't be attributed (no `ss`/`lsof`, or none to check).
    Running,
}

impl Health {
    pub fn is_ok(&self) -> bool {
        matches!(self, Health::Serving(_) | Health::Running)
    }

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

/// `ws_owner`: who is listening on port `$1` — `ours` (pid in tree `$2`), `other`, `none`, `unknown`. Missing `ss`/`lsof` ⇒ `unknown`; visible listener but owner invisible ⇒ `other`.
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

/// Probe what the host process is really doing, in one round trip.
pub fn health(site: &Site, name: &str, ports: &[u16]) -> Option<Health> {
    let (_, pid, _) = paths(name);
    let read = read_pidfile(&pid);
    // Skip the loop for an empty list — `for x in ; do` is a syntax error.
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

/// Like [`health`] but gives a just-launched process time to bind — a
/// compile-first server would otherwise read as `Silent`.
pub fn health_settled(
    site: &Site,
    name: &str,
    ports: &[u16],
    wait: std::time::Duration,
) -> Option<Health> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let got = health(site, name, ports);
        // Only Silent is worth retrying — a crash or squatter won't self-resolve.
        if !matches!(got, Some(Health::Silent(_))) || std::time::Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

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
    // Squatted outranks Silent: it explains an unreachable server, Silent doesn't.
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

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed")
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
        let dir = std::env::temp_dir().join(format!("drup-supervise-{tag}-{}-{uniq}", std::process::id()));
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
        assert_eq!(proc_name("feat/foo", "api"), "drup-feat-foo-api");
        assert_eq!(proc_name("plain", "web"), "drup-plain-web");
    }

    #[test]
    fn paths_quote_a_hostile_name() {
        let (_, pid, _) = paths("drup-feat-x$(touch owned)-api");
        assert!(pid.contains("'drup-feat-x$(touch owned)-api.pid'"), "name not quoted: {pid}");
        assert!(pid.starts_with("\"$HOME/.drup-run/\""), "home must still expand: {pid}");
    }

    #[test]
    fn health_reads_every_state_the_probe_can_report() {
        assert_eq!(parse_health("nopid\n"), Some(Health::NotStarted));
        assert_eq!(parse_health("dead\n"), Some(Health::Crashed));
        assert_eq!(parse_health("alive\nport 20000 ours\n"), Some(Health::Serving(vec![20000])));
        assert_eq!(parse_health("alive\nport 20000 none\n"), Some(Health::Silent(vec![20000])));
        assert_eq!(parse_health("alive\nport 20000 other\n"), Some(Health::Squatted(vec![20000])));
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
        // Exercises the real shell script — parser testing can't catch syntax.
        assert_eq!(
            health(&Site::Local, "drup-no-such-worktree-probe-test", &[]),
            Some(Health::NotStarted)
        );
        assert_eq!(
            health(&Site::Local, "drup-no-such-worktree-probe-test", &[20000]),
            Some(Health::NotStarted)
        );
    }

    #[test]
    fn down_kills_the_whole_tree_under_a_shell_that_does_not_split_parameters() {
        // A remote workspace hands this script to the server's *login* shell
        // (`Site::shell`), which is zsh on macOS and on plenty of Linux dev boxes.
        // zsh word-splits unquoted command substitution but not unquoted parameter
        // expansion, so `kill -TERM $victims` passed one "1234 5678" argument,
        // signalled nothing, and fell through to `rm -f` on the pid file: `drup down`
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
    fn tree_reaches_grandchildren_under_an_awk_that_rejects_newline_in_v() {
        // Regression: `print $1` emitted newline-separated pids that bsd awk
        // (macOS) then rejected via `-v w=`, truncating the BFS at depth 1.
        let mut wrapper = std::process::Command::new("sh")
            .args(["-c", "sh -c 'sleep 30 ; :' ; :"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a depth-2 wrapper");
        let outer = wrapper.id();
        let middle = child_of(outer).expect("outer sh should have a child sh");
        let inner = child_of(middle).expect("inner sh should have a sleep child");

        let tree: Vec<u32> = sh_capture(&format!("{}\nws_tree {}", tree_fn(), outer))
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();

        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(outer.to_string())
            .arg(middle.to_string())
            .arg(inner.to_string())
            .status();
        let _ = wrapper.wait();

        assert!(tree.contains(&outer), "outer {outer} missing from ws_tree: {tree:?}");
        assert!(tree.contains(&middle), "middle {middle} missing from ws_tree: {tree:?}");
        assert!(
            tree.contains(&inner),
            "grandchild {inner} missing from ws_tree: {tree:?} — awk likely errored on \
             newline-in-string and truncated the BFS"
        );
    }

    #[test]
    #[cfg(unix)]
    fn port_attribution_sees_individual_pids_in_every_shell() {
        // `ws_owner`'s two pid lists used to be iterated as bare `$pids`/`$2`, which
        // a login shell that doesn't split parameters (zsh) hands over as a single
        // word — so a healthy host process with more than one pid never matched and
        // `drup list` reported "held by another process" for its own server.
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
                    .args(["-c", script.as_str(), "drup-owner-test", port, tree])
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
