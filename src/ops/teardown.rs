//! Worktree teardown pipeline shared by `ws rm` and `ws clean`.
//!
//! Both ops end the same way: run each repo's `down` then `post-remove` hook,
//! drop the dep symlinks, `git worktree remove`, delete the local branch, and
//! prune the now-empty task dir. `ws rm` gates this behind a dirty/unpushed
//! safety check; `ws clean` gates it behind a merged/clean audit. The gating
//! differs — the teardown itself does not.

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

/// Tear down every worktree of a task. Runs teardown hooks, removes the
/// worktrees, deletes the branch, and prunes the task dir.
///
/// Returns `Ok(true)` when a hook exited non-zero (the worktrees are still
/// removed — resource leakage is the lesser evil), `Ok(false)` when clean.
/// Errors only when `git worktree remove` itself fails even with `--force`.
pub fn remove_task(
    root: &Path,
    cfg: &WsConfig,
    branch: &str,
    worktrees: &[(String, PathBuf)],
) -> Result<bool> {
    let sanitized = workspace::sanitize_branch(branch);
    let mut hook_failed = false;

    // Where the stack lives. `None` ⇒ a remote workspace whose server is
    // unreachable — teardown then skips the server-side steps but still drops the
    // local worktrees (you must be able to `ws rm` even with the server down).
    let site = Site::resolve(cfg).ok();

    for (repo, wt) in worktrees {
        super::bail_if_interrupted()?;

        // Run the resource-teardown lifecycle where the stack lives. A host
        // process's lifecycle IS a detached ws-owned process, so teardown stops it
        // (at its site — local, or the server over SSH) instead of running
        // hooks (the same exclusive split as `ws down`). A remote container runs
        // its down/post-remove scripts on the server; a local container runs them
        // as hooks here.
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
                // Remote server unreachable — can't stop the unit now; still drop
                // the local worktree below so `ws rm` isn't blocked.
                None => crate::ui::skip(repo, "server unreachable — skipping host stop"),
            }
        } else if cfg.remote.is_some() {
            teardown_remote(cfg, site.as_ref(), root, wt, branch, &sanitized, repo, &mut hook_failed);
        } else {
            // Read the existing allocation (never allocate) for the hook env.
            let extra_env =
                inject::prepare(cfg, &Site::Local, root, wt, &sanitized, repo, inject::Phase::Inspect)
                    .unwrap_or_default();

            // Resource teardown hooks — best-effort. A failing hook is reported
            // but doesn't stop removal (the user wants the worktree gone).
            for event in ["down", "post-remove"] {
                if let Some(action) = hooks::resolve(cfg, root, repo, event) {
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

        // Remove this worktree's Caddy routes before `forget` drops the recorded
        // service set they're derived from. Best-effort: Caddy may be down, or (for
        // a remote workspace) the server unreachable.
        if cfg.routing_active() {
            if let Some(site) = site.as_ref() {
                for r in inject::routes(cfg, root, &sanitized, repo) {
                    crate::caddy::route_down(site, &r.id);
                }
            }
        }

        // Drop the generated compose override on the server (the local one is
        // removed by `ports::forget` just below).
        if let Some(s @ Site::Remote { .. }) = site.as_ref() {
            s.remove_file(&inject::override_path_at(cfg, s, root, &sanitized, repo));
        }

        // Drop any localhost→server port forwards before `ports::forget` wipes
        // the record of which ports were allocated. Best-effort, remote-only.
        if cfg.remote.is_some() {
            for (_, port) in ports::load(root, &sanitized, repo) {
                let _ = mutagen::forward_terminate(&mutagen::forward_session_name(branch, repo, port));
            }
        }

        // Free the host ports this worktree held, now that its teardown hooks
        // have run. Done before the worktree removal so a failed `git worktree
        // remove` (which aborts the op) can't leak the `.ws-meta` reservation.
        ports::forget(root, &sanitized, repo);

        // Remote workspace → tear down the file sync before we delete the
        // worktree (so mutagen stops watching a dir that's about to vanish).
        // Best-effort: a failed terminate is reported but doesn't block removal.
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

        // Delete the local branch — best-effort. A branch that's gone (merged
        // and pruned) or checked out elsewhere just leaves this a no-op.
        delete_branch(&main, branch);
    }

    // Best-effort: drop the now-empty task dir. Leftover files (a stray
    // .DS_Store, an unmanaged sibling) make this a silent no-op.
    let _ = std::fs::remove_dir(workspace::task_dir(root, branch));

    Ok(hook_failed)
}

/// Run a remote workspace's `down` + `post-remove` scripts on the server
/// (best-effort). Skips with a note when the server is unreachable — `ws rm` /
/// `ws clean` must still drop the local worktree even when the server is gone.
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
        let Some(cmd) = cfg.script_command(repo, event) else {
            continue;
        };
        let step = crate::ui::step(repo, &format!("{event} hook"));
        match super::run_remote_event(r, root, branch, sanitized, repo, event, &cmd, &extra_env) {
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

/// Remove a worktree, retrying with `--force` when a soft remove refuses
/// (dirty tree, untracked deps). Errors only when even `--force` fails.
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
