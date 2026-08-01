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

        // One rollback site for the whole seeding step: any failure in there
        // leaves a registered-but-half-populated worktree, and the caller's next
        // `ws new` would refuse on the "already exists" conflict.
        if let Err(e) = seed_worktree(repo_entry, repo_name, &repo_path, &wt_path) {
            remove_worktree(&repo_path, &wt_path);
            return Err(e);
        }

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
            let synced = mutagen::create(&session, &wt_path, &r.connection, &remote_path, &ignores)
                // Settle the initial push before we derive/render against the server.
                .and_then(|()| {
                    let _ = mutagen::flush(&session);
                    // Confirm the files actually landed. A session whose transfer
                    // fails still reports `Watching for changes` — the problem shows
                    // only in `mutagen sync list --long` — so without this the step
                    // reports success and the *next* step fails somewhere confusing.
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

    // `--up`: chain straight into the bring-up rather than making the caller run a
    // second command. Delegates to the same op `ws up` runs, so the two can't
    // drift — including its own header, per-repo steps, and failure semantics.
    if up {
        return super::up::run(branch);
    }
    crate::ui::nudge(&format!("ws up {branch}"));

    Ok(())
}

/// Run a repo's `post-create` where its stack lives: on the server over SSH (in
/// the synced worktree) for a remote workspace, or locally. A repo with nothing
/// declared is a no-op. Failure is fatal for `new` (one-time setup that didn't
/// complete).
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
        if let Some(cmd) = hooks::resolve(cfg, ws_root, repo, "post-create", cfg.is_container_repo(repo, wt_path))? {
            let step = crate::ui::step(repo, "Post-create hook");
            hooks::run_command(ws_root, wt_path, repo, sanitized, "post-create", &cmd, extra_env)?;
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
    let _ = git::git(repo)
        .args(["fetch", "origin", "--quiet"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn create_worktree(repo: &Path, branch: &str, wt_path: &Path) -> Result<()> {
    let local_exists = git::ref_exists(repo, &format!("refs/heads/{branch}"));

    let out = if local_exists {
        git::git(repo)
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
        git::git(repo)
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

/// Materialize the fresh worktree's non-git content — the shared `vendor/` copy,
/// the `node_modules` symlink, declared `copy` entries — and register everything
/// ws dropped in the worktree's git exclude.
///
/// Every failure propagates plainly; the caller owns the rollback, so all four
/// steps unwind the same way instead of two of them stranding the worktree.
fn seed_worktree(
    repo_entry: &config::RepoEntry,
    repo_name: &str,
    repo_path: &Path,
    wt_path: &Path,
) -> Result<()> {
    // Artifacts ws drops into the worktree get registered in the worktree's
    // git exclude so they never show as untracked. Seeded with the repo's
    // configured `worktree_excludes`.
    let mut excludes: Vec<String> = repo_entry.worktree_excludes().to_vec();

    // Seeding `vendor/` from the main checkout only makes sense for a
    // gitignored, per-host one. A repo that *commits* its vendor/ already has
    // it in the fresh worktree (`git worktree add` checked it out), and
    // `cp -R src dst` onto an existing dst copies *into* it — landing a silent
    // full duplicate at `<wt>/vendor/vendor`, invisible because the `/vendor`
    // exclude below stops git descending, and pushed over SSH on a remote
    // workspace since a tracked vendor/ is source and isn't sync-ignored.
    if repo_path.join("composer.json").is_file()
        && repo_path.join("vendor").is_dir()
        && !wt_path.join("vendor").exists()
    {
        let step = crate::ui::step(repo_name, "vendor/ copied");
        copy_vendor(repo_path, wt_path).context("vendor/ copy failed")?;
        excludes.push("/vendor".to_string());
        step.ok();
    }

    #[cfg(unix)]
    if repo_path.join("node_modules").is_dir() {
        let step = crate::ui::step(repo_name, "node_modules symlinked");
        symlink_node_modules(repo_name, wt_path)?;
        excludes.push("/node_modules".to_string());
        step.ok();
    }

    // Seed declared untracked files (dev certs/keys/env) from the main checkout
    // into the fresh worktree. Ordered before the excludes registration below so
    // the copied paths join them — `copy` seeds files that aren't in git, and one
    // that isn't gitignored either would otherwise leave the worktree reading
    // permanently dirty. Also before the remote sync (so they ride it to the
    // server) and before post-create (so build steps see them).
    let copies = repo_entry.copy();
    if !copies.is_empty() {
        let step = crate::ui::step(repo_name, "files copied");
        let dests = seed_copies(repo_path, wt_path, copies)
            .with_context(|| format!("copy for {repo_name}"))?;
        // Also exclude the copy *sources* in the main checkout. A `copy` source
        // is a ws-managed untracked file (a dev cert, a seed dump) living in
        // main at the same relative path it lands in the worktree — so the
        // worktree excludes double as main's. Best-effort: keeps main's `git
        // status` clean and ws's footprint fully local (nothing committed), but
        // a failure to write main's exclude must not block creating the worktree.
        let _ = git::add_worktree_excludes(repo_path, &dests);
        excludes.extend(dests);
        step.ok();
    }

    // A symlinked node_modules is a *file* to git, so a repo's conventional
    // `node_modules/` (trailing slash = dir-only) rule won't match it —
    // without this the worktree reads as permanently dirty.
    git::add_worktree_excludes(wt_path, &excludes)
        .with_context(|| format!("register worktree excludes for {repo_name}"))
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

/// Seed `entries` (repo-relative files, directories, or globs) from the main
/// checkout `repo` into worktree `wt`. No-clobber: an existing dest is left as-is.
/// A source with no match is skipped (seed-if-present). `..` is refused so a
/// config entry can't escape the repo.
///
/// Returns the anchored git-exclude entries (`/build-dev/cert`) for what it
/// actually copied, so the caller can keep them out of `git status` — a seeded
/// file that the repo doesn't already gitignore would otherwise read as untracked.
fn seed_copies(repo: &Path, wt: &Path, entries: &[String]) -> Result<Vec<String>> {
    let mut excludes = Vec::new();
    for entry in entries {
        if Path::new(entry).components().any(|c| c == Component::ParentDir) {
            bail!("copy entry `{entry}` must not contain `..`");
        }
        // Only `entry` is a glob — escape the repo prefix so a workspace path that
        // happens to contain `*`/`[`/`?` isn't misread as a pattern.
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

/// Seed a single source path (file or directory) into the worktree with
/// **per-file** no-clobber. A directory is walked and merged file-by-file rather
/// than copied as a unit: the worktree already contains any tracked file the dir
/// carries (e.g. a `.gitkeep`), so a whole-directory `exists()` check would skip
/// the dir and strand its *untracked* contents — the exact reason a generated
/// cert under a tracked `certs/` never arrived. Each copied file is recorded as an
/// anchored git-exclude so it stays out of `git status`.
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
    // Anchored to the worktree root (`/certs/local.crt`), matching the
    // `/vendor` + `/node_modules` convention — an unanchored pattern would match
    // the same name at any depth.
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

/// Best-effort rollback of a half-created worktree. Failures are ignored — this
/// only runs while unwinding from an earlier error, and the original error is
/// what the caller propagates.
fn remove_worktree(repo: &Path, wt: &Path) {
    let _ = git::git(repo)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .output();
}

#[cfg(test)]
mod tests {
    use super::{seed_copies, seed_worktree};
    use crate::config::{RepoConfig, RepoEntry};
    use std::fs;
    use std::path::Path;

    fn touch(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// `git init` at `dir`, so `add_worktree_excludes` has a real
    /// `info/exclude` to resolve and append to.
    fn git_init(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
    }

    fn exclude_lines(repo: &Path) -> Vec<String> {
        fs::read_to_string(repo.join(".git/info/exclude"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn seeding_does_every_job_and_registers_one_exclude_set() {
        // Pins the four jobs `seed_worktree` owns as a unit — vendor copy,
        // node_modules symlink, `copy` entries, and the single exclude
        // registration that covers all of them plus the configured ones.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        git_init(&repo);
        git_init(&wt);
        touch(&repo.join("composer.json"), "{}");
        touch(&repo.join("vendor/autoload.php"), "v");
        touch(&repo.join("node_modules/pkg/index.js"), "n");
        touch(&repo.join(".env"), "e");

        let entry = RepoEntry::Extended(RepoConfig {
            name: "api".into(),
            copy: vec![".env".into()],
            worktree_excludes: vec!["/.venv".into()],
            ..Default::default()
        });

        seed_worktree(&entry, "api", &repo, &wt).unwrap();

        assert!(wt.join("vendor/autoload.php").is_file(), "vendor/ seeded from main");
        assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "e");
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(wt.join("node_modules")).unwrap().is_symlink(),
            "node_modules is linked, not copied",
        );

        let lines = exclude_lines(&wt);
        for want in ["/.venv", "/vendor", "/.env"] {
            assert!(lines.iter().any(|l| l == want), "{want} missing from {lines:?}");
        }
        #[cfg(unix)]
        assert!(lines.iter().any(|l| l == "/node_modules"));

        // The copy *sources* are excluded in the main checkout too, so seeding a
        // worktree doesn't leave main reading dirty.
        assert!(exclude_lines(&repo).iter().any(|l| l == "/.env"));
    }

    #[test]
    fn a_failed_seed_propagates_without_touching_the_worktree() {
        // Rollback is the caller's job (one site, uniform across all four steps);
        // `seed_worktree` itself must only report.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        git_init(&repo);
        git_init(&wt);

        let entry = RepoEntry::Extended(RepoConfig {
            name: "api".into(),
            copy: vec!["../secret".into()],
            ..Default::default()
        });

        assert!(seed_worktree(&entry, "api", &repo, &wt).is_err());
        assert!(wt.is_dir(), "the half-seeded worktree is left for the caller to remove");
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

        // Every copied path comes back as an anchored exclude entry, so a seeded
        // file the repo doesn't gitignore can't leave the worktree reading dirty.
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
        // The real bug: a `copy` dir that already exists in the worktree because it
        // holds a tracked file (a `.gitkeep`) must still receive its untracked
        // siblings. A whole-directory no-clobber skipped them — stranding, e.g., a
        // generated cert under a tracked `certs/`.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        touch(&repo.join("certs/.gitkeep"), "");
        touch(&repo.join("certs/local.crt"), "CERT");
        // The worktree already has the tracked .gitkeep (as after `git worktree add`).
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
