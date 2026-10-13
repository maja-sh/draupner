//! Shared teardown for `wsx rm` and `wsx clean` — the gating differs, but from
//! hooks through branch-delete the sequence is identical.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::compose;
use crate::config::WsConfig;
use crate::hooks;
use crate::inject;
use crate::mutagen;
use crate::ports;
use crate::remote::{self, Site};
use crate::workspace;

/// Tear down every worktree of a task. `Ok(true)` signals a hook exited
/// non-zero (worktrees are removed anyway — resource leakage beats a stuck
/// removal); errors only when `git worktree remove` fails even with `--force`.
pub fn remove_task(
    root: &Path,
    cfg: &WsConfig,
    branch: &str,
    worktrees: &[(String, PathBuf)],
    keep_images: bool,
    keep_volumes: bool,
) -> Result<bool> {
    let sanitized = workspace::sanitize_branch(branch);
    let mut hook_failed = false;

    // `None` ⇒ remote server unreachable; still drop the local worktrees so
    // `wsx rm` isn't blocked when the server is down.
    let site = Site::resolve(cfg).ok();

    for (repo, wt) in worktrees {
        super::bail_if_interrupted()?;

        // Host processes are ws-owned units — stop them at their site instead of
        // running hooks (same exclusive split as `wsx down`).
        if cfg.is_host_repo(repo) {
            teardown_host(site.as_ref(), branch, repo, &mut hook_failed);
        } else if cfg.remote.is_some() {
            teardown_remote(cfg, site.as_ref(), root, wt, branch, &sanitized, repo, keep_images, keep_volumes, &mut hook_failed);
        } else {
            teardown_local(cfg, root, wt, &sanitized, repo, keep_images, keep_volumes, &mut hook_failed);
        }

        // Runs after the hooks so a custom `down` without a matching `post-remove` (or a crashed prior run) can't leak the compose project's volumes/containers indefinitely.
        if !cfg.is_host_repo(repo) && cfg.is_container_repo(repo, wt) {
            if let Some(s) = site.as_ref() {
                let project = compose::project_name(&sanitized, repo);
                let step = crate::ui::step(repo, "compose sweep");
                match sweep_compose(s, cfg.runtime(), &project, keep_volumes) {
                    Ok(rep) if rep.is_empty() => step.ok(),
                    Ok(rep) => step.ok_with(&format!("({rep})")),
                    Err(e) => {
                        step.fail(&e.to_string());
                        hook_failed = true;
                    }
                }
            }
        }

        // Drop routes before `ports::forget` wipes the service set they derive from.
        if cfg.routing_active() {
            if let Some(site) = site.as_ref() {
                for r in inject::routes(cfg, root, &sanitized, repo) {
                    crate::caddy::route_down(site, &r.id);
                }
            }
        }

        // Server-side override only; the local one is removed by `ports::forget` below.
        if let Some(s @ Site::Remote { .. }) = site.as_ref() {
            s.remove_file(&inject::override_path_at(cfg, s, root, &sanitized, repo));
        }

        // Kill port forwards before `ports::forget` drops their record.
        if cfg.remote.is_some() {
            for (_, port) in ports::load(root, &sanitized, repo) {
                let _ = mutagen::forward_terminate(&mutagen::forward_session_name(branch, repo, port));
            }
        }

        // Free ports before worktree removal so a `git worktree remove` failure
        // can't leak the `.ws-meta` reservation.
        ports::forget(root, &sanitized, repo);

        // Terminate the sync before deleting the worktree so mutagen isn't
        // watching a directory that's about to vanish.
        if cfg.remote.is_some() {
            let session = mutagen::session_name(branch, repo);
            let step = crate::ui::step(repo, "sync terminated");
            match mutagen::terminate(&session) {
                Ok(()) => step.ok(),
                Err(e) => step.fail(&format!("{e}")),
            }
        }

        // Drop symlinked deps first so `git worktree remove` sees a clean tree.
        remove_if_symlink(&wt.join("node_modules"));
        remove_if_symlink(&wt.join("vendor"));

        let main = root.join(repo);
        let step = crate::ui::step(repo, "worktree removed");
        match remove_worktree(&main, wt) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail("git worktree remove failed");
                return Err(super::op_failure(format!(
                    "{e}; try `git worktree prune` in {}",
                    main.display()
                )));
            }
        }

        delete_branch(&main, branch);
    }

    // Cross-repo `${WS_PORT_<peer>.<svc>}` pre-allocates the peer's port, so a
    // peer that never got a worktree still holds a reservation to free here.
    let torn_down: std::collections::HashSet<&str> =
        worktrees.iter().map(|(r, _)| r.as_str()).collect();
    for repo in cfg.repos.iter().map(|r| r.name()).filter(|r| !torn_down.contains(r)) {
        ports::forget(root, &sanitized, repo);
    }

    let _ = std::fs::remove_dir(workspace::task_dir(root, branch));

    Ok(hook_failed)
}

/// Stop a `kind: host` repo's supervised process at its site (best-effort); a `None` site is a remote server that's unreachable, so `wsx rm` still drops the local worktree.
fn teardown_host(site: Option<&Site>, branch: &str, repo: &str, hook_failed: &mut bool) {
    match site {
        Some(s) => {
            let step = crate::ui::step(repo, "stop");
            match crate::supervise::down(s, &crate::supervise::proc_name(branch, repo)) {
                Ok(()) => step.ok(),
                Err(e) => {
                    step.fail(&format!("{e}"));
                    *hook_failed = true;
                }
            }
        }
        None => crate::ui::skip(repo, "server unreachable — skipping host stop"),
    }
}

/// Run a local workspace's `down` + `post-remove` hooks; best-effort — a failing hook is reported but doesn't stop removal.
#[allow(clippy::too_many_arguments)]
fn teardown_local(
    cfg: &WsConfig,
    root: &Path,
    wt: &Path,
    sanitized: &str,
    repo: &str,
    keep_images: bool,
    keep_volumes: bool,
    hook_failed: &mut bool,
) {
    // Inspect: read the existing allocation, never allocate.
    let extra_env =
        inject::prepare(cfg, &Site::Local, root, wt, sanitized, repo, inject::Phase::Inspect)
            .unwrap_or_default();

    for event in ["down", "post-remove"] {
        let script = match hooks::resolve(cfg, root, repo, event, cfg.is_container_repo(repo, wt)) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => {
                crate::ui::step(repo, &format!("{event} hook")).fail(&format!("{e}"));
                *hook_failed = true;
                continue;
            }
        };
        let env = post_remove_env(&extra_env, event, keep_images, keep_volumes);
        let step = crate::ui::step(repo, &format!("{event} hook"));
        match hooks::run_command(
            root, wt, repo, sanitized, event, &script.cmd, &env, script.interactive,
        ) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail(&format!("{e}"));
                *hook_failed = true;
            }
        }
    }
}

/// Run a remote workspace's `down` + `post-remove` scripts on the server;
/// skipped when unreachable so local teardown can still proceed.
#[allow(clippy::too_many_arguments)]
fn teardown_remote(
    cfg: &WsConfig,
    site: Option<&Site>,
    root: &Path,
    wt: &Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    keep_images: bool,
    keep_volumes: bool,
    hook_failed: &mut bool,
) {
    let Some(site @ Site::Remote { .. }) = site else {
        crate::ui::skip(repo, "server unreachable — skipping remote down/post-remove");
        return;
    };
    let r = cfg.remote.as_ref().expect("remote site ⇒ a remote config");
    let extra_env =
        inject::prepare(cfg, site, root, wt, sanitized, repo, inject::Phase::Inspect).unwrap_or_default();
    for event in ["down", "post-remove"] {
        let Some(cmd) = cfg.command_for(repo, event, cfg.is_container_repo(repo, wt)) else {
            continue;
        };
        let env = post_remove_env(&extra_env, event, keep_images, keep_volumes);
        let step = crate::ui::step(repo, &format!("{event} hook"));
        match super::run_remote_event(site, r, root, branch, sanitized, repo, event, &cmd, &env) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail(&e.to_string());
                *hook_failed = true;
            }
        }
    }
}

/// Scope `WS_KEEP_IMAGES` / `WS_KEEP_VOLUMES` to `post-remove` so a user's declared `down` script can't accidentally read them as signals meant for teardown's later prune phase.
fn post_remove_env(
    base: &[(String, String)],
    event: &str,
    keep_images: bool,
    keep_volumes: bool,
) -> Vec<(String, String)> {
    if event != "post-remove" || (!keep_images && !keep_volumes) {
        return base.to_vec();
    }
    let mut env = base.to_vec();
    if keep_images {
        env.push(("WS_KEEP_IMAGES".to_string(), "1".to_string()));
    }
    if keep_volumes {
        env.push(("WS_KEEP_VOLUMES".to_string(), "1".to_string()));
    }
    env
}

#[derive(Debug, Default, Clone, Copy)]
struct SweepReport {
    containers: usize,
    volumes: usize,
    networks: usize,
}

impl SweepReport {
    fn is_empty(&self) -> bool {
        self.containers == 0 && self.volumes == 0 && self.networks == 0
    }
}

impl std::fmt::Display for SweepReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        let push = |parts: &mut Vec<String>, n: usize, one: &str, many: &str| {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        };
        push(&mut parts, self.containers, "container", "containers");
        push(&mut parts, self.volumes, "volume", "volumes");
        push(&mut parts, self.networks, "network", "networks");
        f.write_str(&parts.join(", "))
    }
}

/// `keep_volumes` gates volumes only — a leaked container pins its image and a leaked network collides with the next `wsx new` on this branch, so those go regardless.
fn sweep_compose(
    site: &Site,
    runtime: &str,
    project: &str,
    keep_volumes: bool,
) -> Result<SweepReport> {
    let rt = remote::shell_quote(runtime);
    let proj = remote::shell_quote(project);
    let label = format!("com.docker.compose.project={proj}");
    let vol_block = if keep_volumes {
        "vn=0; ".to_string()
    } else {
        format!(
            "v=$({rt} volume ls -q --filter label={label} 2>/dev/null); \
             set -- $v; vn=$#; \
             [ $vn -gt 0 ] && {rt} volume rm -f $v >/dev/null 2>&1; :; "
        )
    };
    // Trailing `:` after each `&&` chain keeps the sub-expression exit non-fatal
    // (no `set -e`, but keeps `$?` clean for the final printf).
    let script = format!(
        "c=$({rt} container ls -aq --filter label={label} 2>/dev/null); \
         set -- $c; cn=$#; \
         [ $cn -gt 0 ] && {rt} container rm -f $c >/dev/null 2>&1; :; \
         {vol_block}\
         n=$({rt} network ls -q --filter label={label} 2>/dev/null); \
         set -- $n; nn=$#; \
         [ $nn -gt 0 ] && {rt} network rm $n >/dev/null 2>&1; :; \
         printf '%s %s %s' \"$cn\" \"$vn\" \"$nn\""
    );
    let out = site
        .shell(&script)
        .output()
        .map_err(|e| anyhow::anyhow!("run {runtime} sweep: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{runtime} sweep exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.split_whitespace();
    let parse = |it: &mut std::str::SplitWhitespace| {
        it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0)
    };
    Ok(SweepReport {
        containers: parse(&mut it),
        volumes: parse(&mut it),
        networks: parse(&mut it),
    })
}

fn remove_if_symlink(p: &Path) {
    if let Ok(meta) = p.symlink_metadata() {
        if meta.file_type().is_symlink() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Retries with `--force` when a soft remove refuses (dirty tree, untracked deps).
fn remove_worktree(main: &Path, wt: &Path) -> Result<()> {
    let soft = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["worktree", "remove"])
        .arg(wt)
        .output()?;
    if soft.status.success() {
        return Ok(());
    }
    let hard = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .output()?;
    if hard.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "git worktree remove: {}",
        String::from_utf8_lossy(&hard.stderr).trim()
    )
}

fn delete_branch(main: &Path, branch: &str) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["branch", "-D", branch])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
