use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::hooks;
use crate::workspace;

pub fn run(repos_arg: &str, branch: &str) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    let requested = parse_repos(repos_arg);
    let known: Vec<&str> = cfg.repos.iter().map(|r| r.name()).collect();
    validate_repos(&requested, &known)?;

    let sanitized = workspace::sanitize_branch(branch);
    if sanitized.is_empty() || sanitized.chars().all(|c| c == '.') {
        return Err(super::preflight(format!("invalid branch name: {branch}")));
    }

    crate::ui::header("ws-new", &format!("{repos_arg} {branch}"));

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
                let _ = remove_worktree(&repo_path, &wt_path);
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
        write_worktree_excludes(&wt_path, &excludes)
            .with_context(|| format!("register worktree excludes for {repo_name}"))?;

        let hook_override = repo_entry.hook_override("post-create");
        if let Some(hook_rel) = hooks::lookup(&ws.root, repo_name, "post-create", hook_override) {
            let step = crate::ui::step(repo_name, "Post-create hook");
            hooks::run(&ws.root, &hook_rel, &wt_path, repo_name, &sanitized, "post-create")?;
            step.ok();
        }

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
    let local_exists = git_ref_exists(repo, &format!("refs/heads/{branch}"));

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
        let remote_exists = git_ref_exists(repo, &format!("refs/remotes/origin/{branch}"));
        let start = if remote_exists {
            format!("origin/{branch}")
        } else {
            resolve_default_start(repo)?
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

fn git_ref_exists(repo: &Path, refspec: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", refspec])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn resolve_default_start(repo: &Path) -> Result<String> {
    if let Some(default) = resolve_origin_head(repo) {
        return Ok(default);
    }
    for (refspec, name) in [
        ("refs/remotes/origin/main", "origin/main"),
        ("refs/remotes/origin/master", "origin/master"),
        ("refs/heads/main", "main"),
        ("refs/heads/master", "master"),
    ] {
        if git_ref_exists(repo, refspec) {
            return Ok(name.to_string());
        }
    }
    bail!(
        "unable to determine default branch for {}",
        repo.display()
    )
}

fn resolve_origin_head(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
    full.strip_prefix("refs/remotes/").map(String::from)
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

/// Append entries to the worktree's git exclude file (`info/exclude`),
/// idempotently. Resolves the real exclude path via `git rev-parse` so it works
/// for a linked worktree's `.git` gitdir pointer. Best-effort on lookup — a
/// missing exclude path just skips silently.
fn write_worktree_excludes(wt: &Path, entries: &[String]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .context("spawn git rev-parse --git-path")?;
    if !out.status.success() {
        return Ok(());
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return Ok(());
    }
    let rel = PathBuf::from(&raw);
    let path = if rel.is_absolute() { rel } else { wt.join(rel) };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let present: std::collections::HashSet<&str> =
        existing.lines().map(str::trim).collect();

    let mut content = existing.clone();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    let mut added = false;
    for e in entries {
        if !present.contains(e.as_str()) {
            content.push_str(e);
            content.push('\n');
            added = true;
        }
    }
    if added {
        std::fs::write(&path, content)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn remove_worktree(repo: &Path, wt: &Path) -> Result<()> {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .output();
    Ok(())
}
