use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::workspace;

/// `ws use [branch]`.
///
/// With a branch, check it out in each repo's *main* checkout, detaching any
/// linked worktree that currently owns it first. Without a branch, just report
/// the current branch per repo. This is the minimum-viable port — the vault
/// flags `use` as "largely obsolete", so the legacy env-file rewriting is not
/// carried over (see spec-2).
pub fn run(branch: Option<&str>) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    let repos: Vec<(String, PathBuf)> = cfg
        .repos
        .iter()
        .map(|r| r.name().to_string())
        .map(|n| {
            let p = ws.root.join(&n);
            (n, p)
        })
        .filter(|(_, p)| workspace::is_git_repo(p))
        .collect();

    match branch {
        None => report_current(&repos),
        Some(branch) => switch(&repos, branch),
    }
}

/// No-arg mode: print each repo's current branch and exit. Read-only.
fn report_current(repos: &[(String, PathBuf)]) -> Result<()> {
    crate::ui::header("ws-use", "");
    let lines = repos
        .iter()
        .map(|(name, main)| {
            let b = current_branch(main).unwrap_or_else(|| "(detached)".to_string());
            format!("{name}: {b}")
        })
        .collect::<Vec<_>>();
    crate::ui::summary_ok(&lines);
    Ok(())
}

fn switch(repos: &[(String, PathBuf)], branch: &str) -> Result<()> {
    crate::ui::header("ws-use", branch);

    // Pre-flight: every repo must already know the branch (local or origin), so
    // we don't half-switch the stack and then fail on the last repo.
    let missing: Vec<&str> = repos
        .iter()
        .filter(|(_, main)| !branch_exists(main, branch))
        .map(|(n, _)| n.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(super::preflight(format!(
            "branch `{branch}` not found (locally or on origin) in: {}",
            missing.join(", ")
        )));
    }

    for (name, main) in repos {
        let step = crate::ui::step(name, &format!("checkout {branch}"));
        // If a linked worktree owns the branch, git refuses to check it out in
        // the main repo — detach that worktree first (leaves it in detached
        // HEAD, per vault convention).
        if let Some(wt) = worktree_on_branch(main, branch) {
            detach_worktree(&wt);
        }
        match checkout(main, branch) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail(&format!("{e}"));
                return Err(super::op_failure(format!("checkout failed for {name}: {e}")));
            }
        }
    }

    crate::ui::summary_ok(&[format!("Now on {branch} in {} repo(s)", repos.len())]);
    Ok(())
}

fn current_branch(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!b.is_empty()).then_some(b)
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git_ref_exists(repo, &format!("refs/heads/{branch}"))
        || git_ref_exists(repo, &format!("refs/remotes/origin/{branch}"))
}

fn git_ref_exists(repo: &Path, refspec: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", refspec])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The linked worktree currently checked out on `branch`, if any (never the
/// main checkout itself). Used to detach a worktree that owns the branch we're
/// about to check out in main.
fn worktree_on_branch(main: &Path, branch: &str) -> Option<PathBuf> {
    let main_canon = main.canonicalize().unwrap_or_else(|_| main.to_path_buf());
    let out = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);

    let mut cur_path: Option<PathBuf> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            cur_path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            let b = b.strip_prefix("refs/heads/").unwrap_or(b);
            if b == branch {
                if let Some(path) = &cur_path {
                    let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
                    if canon != main_canon {
                        return Some(path.clone());
                    }
                }
            }
        }
    }
    None
}

fn detach_worktree(wt: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["checkout", "--detach", "--quiet"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn checkout(main: &Path, branch: &str) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["checkout", "--quiet", branch])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
