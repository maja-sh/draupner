use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::git;
use crate::hooks;
use crate::inject;
use crate::mutagen;
use crate::remote;
use crate::workspace;

pub fn run(repos_arg: &str, branch: &str) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // Where the stack lives (local, or the server for a remote workspace). For a
    // remote workspace this fetches the server `$HOME`, so an unreachable server
    // fails here rather than part-way through creating worktrees.
    let site = remote::Site::resolve(&cfg)?;

    let requested = parse_repos(repos_arg);
    let known: Vec<&str> = cfg.repos.iter().map(|r| r.name()).collect();
    validate_repos(&requested, &known)?;

    let sanitized = workspace::sanitize_branch(branch);
    if sanitized.is_empty() || sanitized.chars().all(|c| c == '.') {
        return Err(super::preflight(format!("invalid branch name: {branch}")));
    }

    crate::ui::header("ws-new", &format!("{repos_arg} {branch}"));

    // A container repo's post-create (Activate) writes an override that joins the
    // external `ws-net`; ensure it exists first so a post-create that runs compose
    // (`compose run --rm app composer install`) doesn't fail on a missing network.
    if cfg.routing_active() {
        if let Err(e) = crate::caddy::ensure_net(&site, cfg.runtime()) {
            crate::ui::note(&format!("ws-net: {e}"));
        }
    }

    let task_dir = workspace::task_dir(&ws.root, branch);

    let mut conflicts = Vec::new();
    for repo_name in &requested {
        if task_dir.join(repo_name).exists() {
            conflicts.push(repo_name.as_str());
        }
    }
    if !conflicts.is_empty() {
        return Err(super::op_failure(format!(
            "worktree(s) already exist for: {}; use `ws rm {}` first",
            conflicts.join(", "),
            branch,
        )));
    }

    std::fs::create_dir_all(&task_dir)
        .with_context(|| format!("create {}", task_dir.display()))?;

    let mut wt_paths = Vec::new();

    for repo_name in &requested {
        super::bail_if_interrupted()?;

        let repo_entry = cfg.repos.iter().find(|r| r.name() == repo_name).unwrap();
        let repo_path = ws.root.join(repo_name);
        let wt_path = task_dir.join(repo_name);

        fetch_origin(&repo_path);

        let step = crate::ui::step(repo_name, "Worktree created");
        create_worktree(&repo_path, branch, &wt_path)
            .with_context(|| format!("create worktree for {repo_name}"))?;
        step.ok();

        // Artifacts ws drops into the worktree get registered in the worktree's
        // git exclude so they never show as untracked. Seeded with the repo's
        // configured `worktree_excludes`.
        let mut excludes: Vec<String> = repo_entry.worktree_excludes().to_vec();

        if repo_path.join("composer.json").is_file() && repo_path.join("vendor").is_dir() {
            let step = crate::ui::step(repo_name, "vendor/ copied");
            if let Err(e) = copy_vendor(&repo_path, &wt_path) {
                remove_worktree(&repo_path, &wt_path);
                return Err(e.context("vendor/ copy failed"));
            }
            excludes.push("/vendor".to_string());
            step.ok();
        }

        #[cfg(unix)]
        if repo_path.join("node_modules").is_dir() {
            let step = crate::ui::step(repo_name, "node_modules symlinked");
            symlink_node_modules(repo_name, &wt_path)?;
            excludes.push("/node_modules".to_string());
            step.ok();
        }

        // A symlinked node_modules is a *file* to git, so a repo's conventional
        // `node_modules/` (trailing slash = dir-only) rule won't match it —
        // without this the worktree reads as permanently dirty.
        git::add_worktree_excludes(&wt_path, &excludes)
            .with_context(|| format!("register worktree excludes for {repo_name}"))?;

        // Advisory isolation lint — run once, here, when the worktree's compose
        // file first lands (not on every `ws up`): its constructs are static
        // properties of the committed file. Warn on the ones that silently defeat
        // per-worktree isolation (`container_name`, external volumes). Never
        // blocks `new`; empty for non-compose repos.
        let lint = crate::compose::lint(cfg.runtime(), &wt_path);
        if !lint.is_empty() {
            crate::ui::warn(&format!("{repo_name}: compose isolation opt-outs"), &lint);
        }

        // A remote workspace establishes the file sync FIRST, so the injection
        // (server-side compose derive + override) and post-create below run
        // against files that exist on the server; a local workspace has them
        // already. Ignores (configured + on-disk dep artifacts) keep per-host
        // trees off the wire — shared with `ws remote set`'s backfill.
        if let Some(r) = &cfg.remote {
            let step = crate::ui::step(repo_name, "Sync created");
            let session = mutagen::session_name(branch, repo_name);
            let remote_path = remote::worktree_path(&r.worktree_base(&ws.root), &sanitized, repo_name);
            let ignores = mutagen::sync_ignores(repo_entry.worktree_excludes(), &wt_path);
            match mutagen::create(&session, &wt_path, &r.connection, &remote_path, &ignores) {
                Ok(()) => step.ok(),
                Err(e) => {
                    step.fail(&format!("{e}"));
                    return Err(super::op_failure(format!("mutagen sync for {repo_name}: {e}")));
                }
            }
            // Settle the initial push before we derive/render against the server.
            let _ = mutagen::flush(&session);
        }

        // Render templates + compute the injection env before post-create so
        // one-time setup (composer install, migrate) sees the per-worktree config
        // and `WS_HOST_*`/`WS_PORT_*` values. Runs at the stack's site — derive,
        // override, and port allocation land on the server for a remote workspace.
        let extra_env = inject::prepare(
            &cfg, &site, &ws.root, &wt_path, &sanitized, repo_name, inject::Phase::Activate,
        )?;

        // Post-create runs where the stack lives: over SSH on the server (in the
        // synced worktree) for a remote workspace, or locally.
        run_post_create(&cfg, &site, &ws.root, &wt_path, branch, &sanitized, repo_name, &extra_env)?;

        wt_paths.push((repo_name.clone(), wt_path));
    }

    let summary: Vec<String> = wt_paths
        .iter()
        .map(|(name, path)| format!("{name}: {}", path.display()))
        .collect();
    crate::ui::summary_ok(&summary);
    crate::ui::nudge(&format!("ws up {branch}"));

    Ok(())
}

/// Run a repo's `post-create` where its stack lives: on the server over SSH (in
/// the synced worktree) for a remote workspace, or locally. A remote workspace
/// runs the declarative `post-create` script (a legacy `.ws-hooks` file lives on
/// the host and can't run there); a repo with nothing declared is a no-op.
/// Failure is fatal for `new` (one-time setup that didn't complete).
#[allow(clippy::too_many_arguments)]
fn run_post_create(
    cfg: &config::WsConfig,
    site: &crate::remote::Site,
    ws_root: &Path,
    wt_path: &Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    extra_env: &[(String, String)],
) -> Result<()> {
    if site.is_remote() {
        let Some(cmd) = cfg.script_command(repo, "post-create") else {
            return Ok(());
        };
        let r = cfg.remote.as_ref().expect("remote site ⇒ a remote config");
        let step = crate::ui::step(repo, "Post-create hook");
        // `run_remote_event` flushes the sync first so the just-rendered templates
        // (and the initial sync) are on the server before post-create runs there.
        match super::run_remote_event(r, ws_root, branch, sanitized, repo, "post-create", &cmd, extra_env) {
            Ok(()) => {
                step.ok();
                Ok(())
            }
            Err(e) => {
                step.fail(&e.to_string());
                Err(e)
            }
        }
    } else {
        if let Some(action) = hooks::resolve(cfg, ws_root, repo, "post-create") {
            let step = crate::ui::step(repo, "Post-create hook");
            hooks::run_action(ws_root, wt_path, repo, sanitized, "post-create", &action, extra_env)?;
            step.ok();
        }
        Ok(())
    }
}

fn parse_repos(s: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_string()))
        .map(String::from)
        .collect()
}

fn validate_repos(requested: &[String], known: &[&str]) -> Result<()> {
    let unknown: Vec<&str> = requested
        .iter()
        .filter(|r| !known.contains(&r.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(super::preflight(format!(
            "unknown repo(s): {}. Known repos: {}",
            unknown.join(", "),
            known.join(", "),
        )));
    }
    Ok(())
}

fn fetch_origin(repo: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fetch", "origin", "--quiet"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn create_worktree(repo: &Path, branch: &str, wt_path: &Path) -> Result<()> {
    let local_exists = git::ref_exists(repo, &format!("refs/heads/{branch}"));

    let out = if local_exists {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "add"])
            .arg(wt_path)
            .arg(branch)
            .output()
            .context("spawn git worktree add")?
    } else {
        let remote_exists = git::ref_exists(repo, &format!("refs/remotes/origin/{branch}"));
        let start = if remote_exists {
            format!("origin/{branch}")
        } else {
            git::default_ref(repo).ok_or_else(|| {
                super::op_failure(format!(
                    "unable to determine default branch for {}",
                    repo.display()
                ))
            })?
        };
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "add", "-b", branch])
            .arg(wt_path)
            .arg(&start)
            .output()
            .context("spawn git worktree add")?
    };

    if !out.status.success() {
        bail!(
            "git worktree add: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn copy_vendor(repo: &Path, wt: &Path) -> Result<()> {
    let src = repo.join("vendor");
    let dst = wt.join("vendor");
    let mut cmd = Command::new("cp");
    cmd.arg("-R");
    if cfg!(target_os = "macos") {
        cmd.arg("-c");
    }
    cmd.arg(&src).arg(&dst);
    let out = cmd.output().context("spawn cp for vendor/")?;
    if !out.status.success() {
        bail!(
            "cp vendor/: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_node_modules(repo_name: &str, wt: &Path) -> Result<()> {
    let target = PathBuf::from("../../..").join(repo_name).join("node_modules");
    let link = wt.join("node_modules");
    std::os::unix::fs::symlink(&target, &link)
        .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))?;
    Ok(())
}

/// Best-effort rollback of a half-created worktree. Failures are ignored — this
/// only runs while unwinding from an earlier error, and the original error is
/// what the caller propagates.
fn remove_worktree(repo: &Path, wt: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .output();
}
