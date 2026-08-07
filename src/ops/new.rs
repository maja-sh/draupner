use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::git;
use crate::hooks;
use crate::inject;
use crate::mutagen;
use crate::remote;
use crate::workspace;

pub fn run(repos_arg: &str, branch: &str, up: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // Resolves the server `$HOME` up front so an unreachable remote fails here, not part-way through creating worktrees.
    let site = remote::Site::resolve(&cfg)?;

    let requested = parse_repos(repos_arg);
    let known: Vec<&str> = cfg.repos.iter().map(|r| r.name()).collect();
    validate_repos(&requested, &known)?;

    let sanitized = workspace::sanitize_branch(branch);
    if sanitized.is_empty() || sanitized.chars().all(|c| c == '.') {
        return Err(super::preflight(format!("invalid branch name: {branch}")));
    }

    crate::ui::header("ws-new", &format!("{repos_arg} {branch}"));

    // Create `ws-net` first: a container repo's post-create writes an override that joins it, and any compose invoked there would fail on a missing network.
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

        // Anything ws drops into the worktree gets registered here so it never reads as untracked.
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

        // Runs before the exclude registration (so copied paths join it), before the remote sync (so they ride it up), and before post-create (so build steps see them).
        let copies = repo_entry.copy();
        if !copies.is_empty() {
            let step = crate::ui::step(repo_name, "files copied");
            match seed_copies(&repo_path, &wt_path, copies) {
                Ok(dests) => {
                    // Best-effort: also register the sources in main so its `git status` stays clean, but never block worktree creation on it.
                    let _ = git::add_worktree_excludes(&repo_path, &dests);
                    excludes.extend(dests);
                    step.ok()
                }
                Err(e) => {
                    remove_worktree(&repo_path, &wt_path);
                    return Err(e.context(format!("copy for {repo_name}")));
                }
            }
        }

        // A symlinked `node_modules` is a file to git, so a repo's `node_modules/` (dir-only) rule won't match it and the worktree reads dirty without this.
        git::add_worktree_excludes(&wt_path, &excludes)
            .with_context(|| format!("register worktree excludes for {repo_name}"))?;

        // Runs here (not on every `ws up`) because it inspects committed static properties of the compose file; advisory, never blocks.
        let lint = crate::compose::lint(cfg.runtime(), &wt_path);
        if !lint.is_empty() {
            crate::ui::warn(&format!("{repo_name}: compose isolation opt-outs"), &lint);
        }

        // Sync first so the server-side derive/override and post-create below run against files that already exist there.
        if let Some(r) = &cfg.remote {
            let step = crate::ui::step(repo_name, "Sync created");
            let session = mutagen::session_name(branch, repo_name);
            let remote_path = remote::worktree_path(&r.worktree_base(&ws.root), &sanitized, repo_name);
            let ignores = mutagen::sync_ignores(repo_entry.worktree_excludes(), &wt_path);
            let synced = mutagen::create(&session, &wt_path, &r.connection, &remote_path, &ignores)
                // Settle the initial push before we derive/render against the server.
                .and_then(|()| {
                    let _ = mutagen::flush(&session);
                    // A session whose transfer fails still reports `Watching for changes`; without this verification the step lies green and the next step fails somewhere confusing.
                    mutagen::verify_beta(&r.connection, &remote_path)
                });
            match synced {
                Ok(()) => step.ok(),
                Err(e) => {
                    step.fail(&format!("{e}"));
                    return Err(super::op_failure(format!("mutagen sync for {repo_name}: {e}")));
                }
            }
        }

        // Render templates before post-create so one-time setup sees the per-worktree config and `WS_HOST_*`/`WS_PORT_*` values.
        let extra_env = inject::prepare(
            &cfg, &site, &ws.root, &wt_path, &sanitized, repo_name, inject::Phase::Activate,
        )?;

        run_post_create(&cfg, &site, &ws.root, &wt_path, branch, &sanitized, repo_name, &extra_env)?;

        wt_paths.push((repo_name.clone(), wt_path));
    }

    let summary: Vec<String> = wt_paths
        .iter()
        .map(|(name, path)| format!("{name}: {}", path.display()))
        .collect();
    crate::ui::summary_ok(&summary);

    // Delegates to the same op `ws up` runs so the two can't drift.
    if up {
        return super::up::run(branch);
    }
    crate::ui::nudge(&format!("ws up {branch}"));

    Ok(())
}

/// Runs a repo's `post-create` where its stack lives (remote over SSH, else local); no-op if nothing is declared.
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
        // `run_remote_event` flushes the sync first so the rendered templates are on the server before the hook runs.
        match super::run_remote_event(site, r, ws_root, branch, sanitized, repo, "post-create", &cmd, extra_env) {
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
        if let Some(action) = hooks::resolve(cfg, ws_root, repo, "post-create", cfg.is_container_repo(repo, wt_path)) {
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
    cp_r(&repo.join("vendor"), &wt.join("vendor")).context("cp vendor/")
}

/// Recursively copy `src` to `dst` via the platform `cp` (clonefile on macOS, so
/// a large `vendor/` is a cheap reflink rather than a byte copy).
fn cp_r(src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new("cp");
    cmd.arg("-R");
    if cfg!(target_os = "macos") {
        cmd.arg("-c");
    }
    cmd.arg(src).arg(dst);
    let out = cmd.output().context("spawn cp")?;
    if !out.status.success() {
        bail!("cp {}: {}", src.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Seed `entries` (files, directories, globs) from `repo` into `wt` with no-clobber, returning anchored git-exclude entries for the paths actually copied.
fn seed_copies(repo: &Path, wt: &Path, entries: &[String]) -> Result<Vec<String>> {
    let mut excludes = Vec::new();
    for entry in entries {
        if Path::new(entry).components().any(|c| c == Component::ParentDir) {
            bail!("copy entry `{entry}` must not contain `..`");
        }
        // Escape the repo prefix so a workspace path containing `*`/`[`/`?` isn't misread as glob syntax.
        let pattern = format!("{}/{entry}", glob::Pattern::escape(&repo.to_string_lossy()));
        let matches =
            glob::glob(&pattern).with_context(|| format!("invalid copy pattern `{entry}`"))?;
        for src in matches.flatten() {
            let rel = src.strip_prefix(repo).expect("a glob match lies under its root");
            seed_one(&src, rel, wt, &mut excludes)?;
        }
    }
    Ok(excludes)
}

/// Per-file no-clobber: a directory is merged rather than treated as a unit, so a `certs/` that already exists for a tracked `.gitkeep` still receives its untracked siblings.
fn seed_one(src: &Path, rel: &Path, wt: &Path, excludes: &mut Vec<String>) -> Result<()> {
    if src.is_dir() {
        for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let entry = entry?;
            seed_one(&entry.path(), &rel.join(entry.file_name()), wt, excludes)?;
        }
        return Ok(());
    }
    let dst = wt.join(rel);
    if dst.exists() {
        return Ok(()); // no-clobber — a worktree's local edit wins
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    cp_r(src, &dst)?;
    // Anchored so it can't match the same name at another depth.
    excludes.push(format!("/{}", rel.display()));
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

/// Best-effort rollback while unwinding an earlier error; failures are ignored because the original error is what the caller propagates.
fn remove_worktree(repo: &Path, wt: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .output();
}

#[cfg(test)]
mod tests {
    use super::seed_copies;
    use std::fs;
    use std::path::Path;

    fn touch(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn copies_file_directory_and_glob_only_matching() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        touch(&repo.join(".env"), "e");
        touch(&repo.join("build-dev/cert/a.pem"), "c");
        touch(&repo.join("storage/oauth-private.key"), "k");
        touch(&repo.join("storage/other.txt"), "o");

        let excludes = seed_copies(
            &repo,
            &wt,
            &[".env".into(), "build-dev/cert".into(), "storage/oauth-*.key".into()],
        )
        .unwrap();

        assert!(wt.join(".env").is_file());
        assert!(wt.join("build-dev/cert/a.pem").is_file(), "directory copied recursively");
        assert!(wt.join("storage/oauth-private.key").is_file());
        assert!(!wt.join("storage/other.txt").exists(), "glob must not pull non-matches");

        let mut got = excludes.clone();
        got.sort();
        // Per-file excludes (a directory is merged file-by-file, not as a unit).
        assert_eq!(
            got,
            vec!["/.env", "/build-dev/cert/a.pem", "/storage/oauth-private.key"],
        );
    }

    #[test]
    fn merges_into_an_existing_dir_seeding_its_untracked_files() {
        // Regression: a `copy` dir that already exists for a tracked `.gitkeep` must still receive its untracked siblings.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        touch(&repo.join("certs/.gitkeep"), "");
        touch(&repo.join("certs/local.crt"), "CERT");
        touch(&wt.join("certs/.gitkeep"), "");

        let excludes = seed_copies(&repo, &wt, &["certs".into()]).unwrap();

        assert_eq!(
            fs::read_to_string(wt.join("certs/local.crt")).unwrap(),
            "CERT",
            "the untracked cert must be seeded despite the dir already existing",
        );
        assert_eq!(excludes, vec!["/certs/local.crt"], "only the newly-copied file");
    }

    #[test]
    fn no_clobber_keeps_the_worktrees_version() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        touch(&repo.join(".env"), "from-main");
        touch(&wt.join(".env"), "local-edit");

        let excludes = seed_copies(&repo, &wt, &[".env".into()]).unwrap();

        assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "local-edit");
        assert!(excludes.is_empty(), "a skipped copy contributes no exclude entry");
    }

    #[test]
    fn missing_source_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&wt).unwrap();

        let excludes =
            seed_copies(&repo, &wt, &["nope.env".into(), "missing/*.key".into()]).unwrap();

        assert!(!wt.join("nope.env").exists());
        assert!(excludes.is_empty());
    }

    #[test]
    fn parent_escape_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&wt).unwrap();

        assert!(seed_copies(&repo, &wt, &["../secret".into()]).is_err());
        assert!(seed_copies(&repo, &wt, &["a/../../secret".into()]).is_err());
    }
}
