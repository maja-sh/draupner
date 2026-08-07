//! Shared teardown for `ws rm` and `ws clean` — the gating differs, but from
//! hooks through branch-delete the sequence is identical.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::WsConfig;
use crate::hooks;
use crate::inject;
use crate::mutagen;
use crate::ports;
use crate::remote::Site;
use crate::workspace;

/// Tear down every worktree of a task. `Ok(true)` signals a hook exited
/// non-zero (worktrees are removed anyway — resource leakage beats a stuck
/// removal); errors only when `git worktree remove` fails even with `--force`.
pub fn remove_task(
    root: &Path,
    cfg: &WsConfig,
    branch: &str,
    worktrees: &[(String, PathBuf)],
) -> Result<bool> {
    let sanitized = workspace::sanitize_branch(branch);
    let mut hook_failed = false;

    // `None` ⇒ remote server unreachable; still drop the local worktrees so
    // `ws rm` isn't blocked when the server is down.
    let site = Site::resolve(cfg).ok();

    for (repo, wt) in worktrees {
        super::bail_if_interrupted()?;

        // Host processes are ws-owned units — stop them at their site instead of
        // running hooks (same exclusive split as `ws down`).
        if cfg.is_host_repo(repo) {
            match site.as_ref() {
                Some(s) => {
                    let step = crate::ui::step(repo, "stop");
                    match crate::supervise::down(s, &crate::supervise::proc_name(branch, repo)) {
                        Ok(()) => step.ok(),
                        Err(e) => {
                            step.fail(&format!("{e}"));
                            hook_failed = true;
                        }
                    }
                }
                None => crate::ui::skip(repo, "server unreachable — skipping host stop"),
            }
        } else if cfg.remote.is_some() {
            teardown_remote(cfg, site.as_ref(), root, wt, branch, &sanitized, repo, &mut hook_failed);
        } else {
            // Inspect: read the existing allocation, never allocate.
            let extra_env =
                inject::prepare(cfg, &Site::Local, root, wt, &sanitized, repo, inject::Phase::Inspect)
                    .unwrap_or_default();

            // Best-effort: a failing hook is reported but doesn't block removal.
            for event in ["down", "post-remove"] {
                if let Some(action) = hooks::resolve(cfg, root, repo, event, cfg.is_container_repo(repo, wt)) {
                    let step = crate::ui::step(repo, &format!("{event} hook"));
                    match hooks::run_action(root, wt, repo, &sanitized, event, &action, &extra_env) {
                        Ok(()) => step.ok(),
                        Err(e) => {
                            step.fail(&format!("{e}"));
                            hook_failed = true;
                        }
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
        let step = crate::ui::step(repo, &format!("{event} hook"));
        match super::run_remote_event(site, r, root, branch, sanitized, repo, event, &cmd, &extra_env) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail(&e.to_string());
                *hook_failed = true;
            }
        }
    }
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
