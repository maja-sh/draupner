use anyhow::Result;

use crate::config;
use crate::remote;
use crate::ui::{classify_status, StatusKind};
use crate::workspace;

pub fn run(repos_arg: &str, branch: &str, up: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // Resolves the server `$HOME` up front — an unreachable remote can't back a new sync.
    let site = remote::Site::resolve(&cfg)?;

    let requested = super::new::parse_repos(repos_arg);
    if requested.is_empty() {
        return Err(super::preflight("no repos specified"));
    }
    let known: Vec<&str> = cfg.repos.iter().map(|r| r.name()).collect();
    super::new::validate_repos(&requested, &known)?;

    let sanitized = workspace::validate_branch(branch).map_err(super::preflight)?;

    let task_dir = workspace::task_dir(&ws.root, branch);
    if !task_dir.is_dir() {
        return Err(super::preflight(format!(
            "no worktrees for {branch}; run `ws new` first"
        )));
    }

    crate::ui::header("ws-add", &format!("{repos_arg} {branch}"));

    // Cheap preflights first — a typo shouldn't pay for a Caddy call, an
    // orphan scan (git worktree list per configured repo), or a status
    // probe (potentially one SSH round-trip per existing repo).
    let conflicts = workspace::existing_repos_in_task(&task_dir, &requested);
    if !conflicts.is_empty() {
        return Err(super::op_failure(format!(
            "repo(s) already in `{branch}`: {}",
            conflicts.join(", "),
        )));
    }

    // Same legacy-hook migration gate `ws new` raises — only for the repos
    // being added; the task's existing repos already crossed it at `ws new`.
    crate::hooks::preflight_legacy_hook_files(&cfg, &ws.root, &requested, &["post-create"])
        .map_err(|e| super::preflight(e.to_string()))?;

    // Same as `ws new`: a container repo's post-create may write an override that joins ws-net.
    super::ensure_ws_net_if_active(&cfg, &site);

    // An orphan under this task_dir (a `.worktrees/<task>/<repo>/` git doesn't
    // know) means the task is already in a broken state that adding a repo
    // would only compound.
    let known_wts = workspace::worktrees(&ws.root, &cfg);
    let orphans: Vec<String> = workspace::orphan_dirs(&ws.root, &known_wts)
        .into_iter()
        .filter(|p| p.starts_with(&task_dir))
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    if !orphans.is_empty() {
        return Err(super::preflight(format!(
            "task `{branch}` has orphan worktree dir(s) ({}); run `ws clean` first",
            orphans.join(", "),
        )));
    }

    // `down → add → up` is the intended order. Anything not classifiable as
    // Stopped — running, crashed, or a probe ws can't read — leaves state
    // (`.ws-meta` ports, overrides, forwards, live containers) that would
    // race a new repo's post-create.
    require_task_stopped(&cfg, &site, &ws.root, branch, &sanitized)?;

    super::new::add_repos(&ws.root, &cfg, &site, branch, &sanitized, &requested)?;

    if up {
        return super::up::run(branch);
    }
    crate::ui::nudge(&format!("ws up {branch}"));
    Ok(())
}

/// Refuse the op only for repos the probe classifies as running/degraded/crashed.
/// A `None` from probe_status (plain repo with nothing to probe, hook returned
/// nothing) or `None` from classify (garbage output) both default to allowing —
/// the false-negative failure mode is real state corruption, but a hook that
/// can't say "running" isn't diagnostic of a live stack.
fn require_task_stopped(
    cfg: &config::WsConfig,
    site: &remote::Site,
    ws_root: &std::path::Path,
    branch: &str,
    sanitized: &str,
) -> Result<()> {
    let mut live: Vec<String> = Vec::new();
    for (repo, wt) in workspace::task_worktrees(ws_root, cfg, branch) {
        let (status, notes) =
            super::list::probe_status(cfg, Some(site), ws_root, &wt, branch, sanitized, &repo);
        for n in notes {
            crate::ui::note(&n);
        }
        match status.as_deref().and_then(classify_status) {
            Some(StatusKind::Healthy | StatusKind::Degraded | StatusKind::Crashed) => {
                live.push(repo);
            }
            Some(StatusKind::Stopped) | None => {}
        }
    }
    if live.is_empty() {
        return Ok(());
    }
    Err(super::preflight(format!(
        "task `{branch}` is not fully stopped (live: {}); run `ws down {branch}` first",
        live.join(", "),
    )))
}
