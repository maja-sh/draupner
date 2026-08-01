use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{RemoteConfig, RepoConfig, RepoEntry, WsConfig};
use crate::compose;

/// The lifecycle events `ws` dispatches, in the order they happen. `ws init`
/// scaffolds one starter script per repo per event; declaring one under a repo's
/// `scripts` is what turns it on.
const HOOK_EVENTS: &[&str] =
    &["prepare", "post-create", "up", "down", "post-remove", "status"];

pub fn run(
    refresh_schema: bool,
    remote: Option<Option<String>>,
    remote_path: Option<String>,
    domain: Option<String>,
    runtime: Option<String>,
    no_verify: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;

    if refresh_schema {
        // Resolve the workspace the way every other op does — by walking up — rather
        // than demanding the cwd *be* the root. Scaffolding a new workspace is
        // rightly cwd-bound; refreshing an existing one is the same "which workspace
        // am I in" question `ws up`/`list`/`exec` already answer from any
        // subdirectory, and answering it differently here is just a trap.
        let root = crate::workspace::locate()
            .map(|ws| ws.root)
            .map_err(|_| super::preflight(format!(
                "no `.ws.json` in {} or any parent — `ws init --refresh-schema` \
                 only runs inside an initialized workspace.",
                cwd.display()
            )))?;
        return refresh_schema_only(&root);
    }

    let remote_cfg = build_remote(&cwd, remote, remote_path)?;
    init_local(&cwd, remote_cfg, domain, runtime, no_verify)
}

/// Turn the `--remote[=host]` / `--remote-path` flags into a `RemoteConfig`, or
/// `None` when `--remote` wasn't given. A bare `--remote` falls back to
/// `$WS_REMOTE_HOST`, then the global default (`ws config set remote.host`).
/// `workspace_path` is written explicitly (defaulting to the workspace dir
/// name) so it's visible in `.ws.json`. `--domain` is handled separately —
/// it's top-level now (routing is location-transparent).
fn build_remote(
    root: &Path,
    remote: Option<Option<String>>,
    remote_path: Option<String>,
) -> Result<Option<RemoteConfig>> {
    let Some(conn) = remote else { return Ok(None) };
    let connection = conn
        .or_else(super::default_remote_host)
        .ok_or_else(|| {
            super::preflight(
                "`ws init --remote` needs a host: pass `--remote <host>`, set $WS_REMOTE_HOST, \
                 or `ws config set remote.host <host>`",
            )
        })?;
    // The flag path is fresh user input — refuse an option-shaped host here,
    // not just when a stored config is later loaded.
    crate::remote::ensure_connection_safe(&connection).map_err(|e| super::preflight(e.to_string()))?;
    // An empty --remote-path (a script's unset variable) is treated as absent —
    // storing "" would anchor the server layout at the filesystem root.
    let workspace_path = remote_path
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| crate::config::default_worktree_base(root));
    Ok(Some(RemoteConfig::new(connection, Some(workspace_path))))
}

fn init_local(
    root: &Path,
    remote: Option<RemoteConfig>,
    domain: Option<String>,
    runtime: Option<String>,
    no_verify: bool,
) -> Result<()> {
    crate::ui::header("ws-init", &format!("in {}", root.display()));

    let runtime = runtime.unwrap_or_else(|| "podman".to_string());

    // Blocks *creating* a nested root, not running `ws` from a subdir (that
    // still resolves upward). The hazard: under the outer's `.worktrees/`, an
    // outer `ws clean`/`rm` would tear the inner workspace down.
    if let Some(outer) = enclosing_workspace(root) {
        let detail = if root.starts_with(outer.join(".worktrees")) {
            " — inside its `.worktrees/` tree, where an outer `ws clean`/`rm` would tear this down"
        } else {
            ""
        };
        return Err(super::preflight(format!(
            "refusing to initialize: already inside the workspace at {}{}. \
             Nested workspaces aren't supported — run `ws init` outside it.",
            outer.display(),
            detail,
        )));
    }

    let cfg_path = root.join(".ws.json");
    if cfg_path.exists() {
        bail!(
            "{} already exists. Remove it (or use `--refresh-schema` to regenerate the JSON Schema).",
            cfg_path.display()
        );
    }

    // Verify client readiness before writing anything, so a strict-mode refusal
    // leaves no half-written config behind.
    if let Some(r) = &remote {
        super::verify_remote_client(r, no_verify)?;
    }

    let repos = detect_repos(root)?;

    let remote_note = remote.as_ref().map(|r| r.connection.clone());
    let cfg = WsConfig {
        schema: Some("./.ws.schema.json".to_string()),
        domain,
        // Written explicitly (like workspace_path) so the routing runtime is
        // visible/discoverable in the scaffolded .ws.json.
        runtime: Some(runtime.clone()),
        remote,
        repos: repos.iter().map(|r| scaffold_repo(root, r, &runtime)).collect(),
    };
    super::write_ws_json(root, &cfg)?;
    write_schema(root)?;

    let step_hooks = crate::ui::step("hks", ".ws-hooks/ templates");
    write_hook_templates(&root.join(".ws-hooks"), &repos)?;
    step_hooks.ok();

    let mut summary = Vec::new();
    summary.push(format!(".ws.json  ({} repos: {})", repos.len(), repos.join(", ")));
    if let Some(conn) = &remote_note {
        summary.push(format!("remote  → {conn}"));
    }
    summary.push(".ws.schema.json".to_string());
    summary.push(".ws-hooks/".to_string());
    crate::ui::summary_ok(&summary);
    if remote_note.is_some() {
        crate::ui::nudge("run `ws remote check` once the server's set up for full validation");
    } else {
        crate::ui::nudge("edit .ws.json / .ws-hooks/, then `ws claude`");
    }
    Ok(())
}

fn refresh_schema_only(root: &Path) -> Result<()> {
    crate::ui::header("ws-init", "--refresh-schema");

    // `root` is already a located workspace (the caller walked up to find it), so
    // the schema can't land in a non-workspace dir.
    write_schema(root)?;
    crate::ui::summary_ok(&[format!("{} refreshed", root.join(".ws.schema.json").display())]);
    Ok(())
}

/// Generate `<root>/.ws.schema.json` from the live `WsConfig`, step UI included,
/// so `ws init` and `ws init --refresh-schema` can't drift on the filename or
/// the step label.
fn write_schema(root: &Path) -> Result<()> {
    let step = crate::ui::step("cfg", ".ws.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    crate::config::write_json_pretty(&root.join(".ws.schema.json"), &schema)?;
    step.ok();
    Ok(())
}

/// The nearest workspace strictly above `root` (an ancestor holding `.ws.json`),
/// or `None` if `root` isn't nested inside one.
fn enclosing_workspace(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .skip(1)
        .find(|dir| dir.join(".ws.json").is_file())
        .map(Path::to_path_buf)
}

/// Build a repo entry, scaffolding the auto-derived `services` block when the
/// repo is a compose project so the routing is visible in `.ws.json` from day
/// one. Best-effort: if the repo has no compose file, or `compose config` can't
/// run (runtime absent), it stays a bare-name entry — `ws up` re-derives live
/// regardless.
fn scaffold_repo(root: &Path, name: &str, runtime: &str) -> RepoEntry {
    let repo_dir = root.join(name);
    // `ws init` scaffolds from the local checkout on the host (there's no server
    // yet — the workspace is being created), so derivation runs on the local site.
    let services = compose::has_compose_file(&repo_dir)
        .then(|| {
            compose::derive(&crate::remote::Site::Local, runtime, &repo_dir.to_string_lossy()).ok().map(|d| d.services)
        })
        .flatten()
        .filter(|s| !s.is_empty());
    match services {
        Some(services) => {
            RepoEntry::Extended(RepoConfig { name: name.to_string(), services, ..Default::default() })
        }
        None => RepoEntry::Name(name.to_string()),
    }
}

fn detect_repos(root: &Path) -> Result<Vec<String>> {
    // `list_child_dirs` swallows a failing `read_dir` and returns an empty vec,
    // which for `ws init` would scaffold a repo-less `.ws.json` off an unreadable
    // cwd instead of saying so. Probe first, then share the walk.
    fs::read_dir(root).context("read cwd")?;

    let mut names = Vec::new();
    // The walk itself is shared with the rest of `ws` (`workspace::list_child_dirs`):
    // it swallows per-entry read errors so one broken symlink doesn't abort the
    // scan, and it skips symlinks (they can point outside the workspace and
    // confuse later ops) and anything that isn't a real dir. Only the dotfile
    // skip and the `is_git_repo` filter are `init`-specific.
    for (name, path) in crate::workspace::list_child_dirs(root) {
        let Some(name) = name else {
            // Non-UTF-8 dir names would round-trip through `.ws.json` lossily
            // and break later worktree lookups. Skip with a warn instead of
            // silently corrupting the config.
            tracing::warn!(
                path = %path.display(),
                "skipping non-UTF-8 directory name in workspace scan"
            );
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if crate::workspace::is_git_repo(&path) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Scaffold a starter shell script per repo per event under `.ws-hooks/`, as
/// `<repo>-<event>.sh`. They're inert until a repo's `scripts` block names one —
/// `ws` runs only what `.ws.json` declares — so the template body says how.
/// Written executable, because a `scripts` entry invokes the file directly.
fn write_hook_templates(dir: &Path, repos: &[String]) -> Result<()> {
    fs::create_dir_all(dir)?;
    for repo in repos {
        for event in HOOK_EVENTS {
            let path = dir.join(hook_template_name(repo, event));
            if !path.exists() {
                fs::write(&path, hook_template(repo, event))?;
                make_executable(&path)?;
            }
        }
    }
    Ok(())
}

fn hook_template_name(repo: &str, event: &str) -> String {
    format!("{repo}-{event}.sh")
}

fn hook_template(repo: &str, event: &str) -> String {
    let name = hook_template_name(repo, event);
    format!(
        "#!/usr/bin/env bash\n\
         # {repo}'s {event} script. Runs from the worktree; its path is $WS_WORKTREE.\n\
         # Enable it by adding to {repo}'s `scripts` in .ws.json:\n\
         #   \"{event}\": \".ws-hooks/{name}\"\n\
         set -euo pipefail\n\
         \n\
         echo \"{repo}-{event}: worktree=$WS_WORKTREE\"\n"
    )
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod +x {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}
