//! Worktree teardown shared by `ws rm` and `ws clean`. Only their gating
//! differs (dirty/unpushed check vs merged/clean audit); the teardown does not.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::compose;
use crate::config::WsConfig;
use crate::git;
use crate::inject;
use crate::mutagen;
use crate::ports;
use crate::remote::Site;
use crate::workspace;

/// Tear down every worktree of a task.
///
/// Returns `Ok(true)` when a hook exited non-zero — the worktrees are removed
/// anyway, since leaked resources beat a task you can't delete. Errors only
/// when `git worktree remove` fails even with `--force`.
pub fn remove_task(
    root: &Path,
    cfg: &WsConfig,
    branch: &str,
    worktrees: &[(String, PathBuf)],
) -> Result<bool> {
    let sanitized = workspace::sanitize_branch(branch);
    let mut hook_failed = false;

    // `None` ⇒ a remote workspace whose server is unreachable: skip the
    // server-side steps but still drop the local worktrees, because `ws rm` has
    // to work with the server down.
    let site = Site::resolve(cfg).ok();

    for (repo, wt) in worktrees {
        super::bail_if_interrupted()?;

        // A host repo's lifecycle *is* a detached ws-owned process, so it gets
        // stopped rather than hooked — the same exclusive split as `ws down`.
        if cfg.is_host_repo(repo) {
            teardown_host(site.as_ref(), branch, repo, &mut hook_failed);
        } else {
            teardown_scripts(cfg, site.as_ref(), root, wt, branch, &sanitized, repo, &mut hook_failed);
        }

        // Before `ports::forget` drops the recorded service set the routes are
        // derived from. Best-effort — Caddy or the server may be down.
        if cfg.routing_active() {
            if let Some(site) = site.as_ref() {
                for r in inject::routes(cfg, root, &sanitized, repo) {
                    crate::caddy::route_down(site, &r.id);
                }
            }
        }

        // The local override is removed by `ports::forget` below; the server's
        // copy is not.
        if let Some(s @ Site::Remote { .. }) = site.as_ref() {
            s.remove_file(&compose::override_path_at(cfg, s, root, &sanitized, repo));
        }

        // Before `ports::forget` wipes the record the forwards are derived from.
        if cfg.remote.is_some() {
            mutagen::terminate_forwards(root, branch, &sanitized, repo);
        }

        // Before the worktree removal, so a failed `git worktree remove` (which
        // aborts the op) can't leak the `.ws-meta` reservation.
        ports::forget(root, &sanitized, repo);

        // Stop mutagen watching a directory that's about to vanish. Best-effort:
        // a failed terminate is reported but doesn't block removal.
        if cfg.remote.is_some() {
            let session = mutagen::session_name(branch, repo);
            let step = crate::ui::step(repo, "sync terminated");
            match mutagen::terminate(&session) {
                Ok(()) => step.ok(),
                Err(e) => step.fail(&format!("{e}")),
            }
        }

        // So `git worktree remove` sees a clean tree.
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

    // A cross-repo `${WS_PORT_<peer>.<svc>}` pre-allocates the peer's port before
    // the peer is built, so a peer that never got a worktree would leak that
    // reservation past teardown. Exact repo names, never a prefix match, so a
    // sibling branch is untouched.
    let torn_down: std::collections::HashSet<&str> =
        worktrees.iter().map(|(r, _)| r.as_str()).collect();
    for repo in cfg.repos.iter().map(|r| r.name.as_str()).filter(|r| !torn_down.contains(r)) {
        ports::forget(root, &sanitized, repo);
    }

    // Non-recursive on purpose: leftover files (a stray .DS_Store, an unmanaged
    // sibling) make this a silent no-op rather than a surprise deletion.
    let _ = std::fs::remove_dir(workspace::task_dir(root, branch));

    Ok(hook_failed)
}

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

/// Run a worktree's `down` + `post-remove` events. Best-effort: a failing event
/// is reported but doesn't stop removal, because the user wants the worktree
/// gone. A `None` site (unreachable server) skips with a note for the same
/// reason.
#[allow(clippy::too_many_arguments)]
fn teardown_scripts(
    cfg: &WsConfig,
    site: Option<&Site>,
    root: &Path,
    wt: &Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    hook_failed: &mut bool,
) {
    let Some(site) = site else {
        crate::ui::skip(repo, "server unreachable — skipping remote down/post-remove");
        return;
    };
    let ctx = crate::lifecycle::Ctx { cfg, site, root, worktree: wt, branch, sanitized, repo };

    // `Inspect` reads the existing allocation; teardown must never allocate.
    let extra_env =
        inject::prepare(cfg, site, root, wt, sanitized, repo, inject::Phase::Inspect)
            .unwrap_or_default();

    for event in ["down", "post-remove"] {
        let cmd = match ctx.command(event) {
            Ok(Some(cmd)) => cmd,
            Ok(None) => continue,
            Err(e) => {
                crate::ui::step(repo, &format!("{event} hook")).fail(&format!("{e}"));
                *hook_failed = true;
                continue;
            }
        };
        let step = crate::ui::step(repo, &format!("{event} hook"));
        match ctx.run(event, &cmd, &extra_env) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail(&format!("{e}"));
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

/// Retries with `--force` because a soft remove refuses on a dirty tree or
/// untracked deps, which a task worktree usually has.
fn remove_worktree(main: &Path, wt: &Path) -> Result<()> {
    let soft = git::git(main)
        .args(["worktree", "remove"])
        .arg(wt)
        .output()?;
    if soft.status.success() {
        return Ok(());
    }
    let hard = git::git(main)
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
    let _ = git::git(main)
        .args(["branch", "-D", branch])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
