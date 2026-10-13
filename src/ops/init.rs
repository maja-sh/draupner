use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{RemoteConfig, RepoConfig, RepoEntry, WsConfig};
use crate::compose;

/// The lifecycle events `wsx` dispatches, in the order they happen. `wsx init`
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
        // Refreshing an existing workspace resolves upward like every other op; only scaffolding is cwd-bound.
        let root = crate::workspace::locate()
            .map(|ws| ws.root)
            .map_err(|_| super::preflight(format!(
                "no `.wsx.json` in {} or any parent — `wsx init --refresh-schema` \
                 only runs inside an initialized workspace.",
                cwd.display()
            )))?;
        return refresh_schema_only(&root);
    }

    let remote_cfg = build_remote(&cwd, remote, remote_path)?;
    init_local(&cwd, remote_cfg, domain, runtime, no_verify)
}

/// Turn the `--remote[=host]` / `--remote-path` flags into a `RemoteConfig`, falling back to `$WSX_REMOTE_HOST` then the stored global default; `None` when `--remote` wasn't given.
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
                "`wsx init --remote` needs a host: pass `--remote <host>`, set $WSX_REMOTE_HOST, \
                 or `wsx config set remote.host <host>`",
            )
        })?;
    // Fresh user input: reject an option-shaped host here, not only when a stored config later loads.
    crate::remote::ensure_connection_safe(&connection).map_err(|e| super::preflight(e.to_string()))?;
    // Empty --remote-path is treated as absent; storing "" would anchor the server layout at the filesystem root.
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

    // Only an explicit `--runtime` is persisted; a detected one stays out of `.wsx.json` so the workspace re-detects wherever it's opened.
    let effective =
        runtime.clone().unwrap_or_else(|| crate::config::detect_runtime().unwrap_or("podman").to_string());

    // Blocks *creating* a nested root (not running `wsx` from a subdir), because an outer `wsx clean`/`rm` would tear an inner workspace down.
    if let Some(outer) = enclosing_workspace(root) {
        let detail = if root.starts_with(outer.join(".worktrees")) {
            " — inside its `.worktrees/` tree, where an outer `wsx clean`/`rm` would tear this down"
        } else {
            ""
        };
        return Err(super::preflight(format!(
            "refusing to initialize: already inside the workspace at {}{}. \
             Nested workspaces aren't supported — run `wsx init` outside it.",
            outer.display(),
            detail,
        )));
    }

    if root.join(".wsx.json").exists() {
        bail!(
            "{} already exists. Remove it (or use `--refresh-schema` to regenerate the JSON Schema).",
            root.join(".wsx.json").display()
        );
    }

    // Verify before writing anything so a strict-mode refusal leaves no half-written config behind.
    if let Some(r) = &remote {
        super::verify_remote_client(r, no_verify)?;
    }

    let repos = detect_repos(root)?;

    let remote_note = remote.as_ref().map(|r| r.connection.clone());
    let pinned = runtime.is_some();
    let step_config = crate::ui::step("cfg", ".wsx.json");
    let cfg = WsConfig {
        schema: Some("./.wsx.schema.json".to_string()),
        domain,
        runtime,
        remote,
        repos: repos.iter().map(|r| scaffold_repo(root, r, &effective)).collect(),
    };
    let cfg_path = root.join(".wsx.json");
    crate::config::write_json_pretty(&cfg_path, &cfg)?;
    step_config.ok();

    let step_schema = crate::ui::step("cfg", ".wsx.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    crate::config::write_json_pretty(&root.join(".wsx.schema.json"), &schema)?;
    step_schema.ok();

    let step_hooks = crate::ui::step("hks", ".wsx-hooks/ templates");
    write_hook_templates(&root.join(".wsx-hooks"), &repos)?;
    step_hooks.ok();

    let mut summary = Vec::new();
    summary.push(format!(".wsx.json  ({} repos: {})", repos.len(), repos.join(", ")));
    if let Some(conn) = &remote_note {
        summary.push(format!("remote  → {conn}"));
    }
    // A detected runtime isn't written to `.wsx.json`, so surface it here or it'd be invisible until something used it.
    summary.push(if pinned {
        format!("runtime → {effective} (pinned in .wsx.json)")
    } else {
        format!("runtime → {effective} (detected — add `runtime` to .wsx.json to pin it)")
    });
    summary.push(".wsx.schema.json".to_string());
    summary.push(".wsx-hooks/".to_string());
    crate::ui::summary_ok(&summary);
    if remote_note.is_some() {
        crate::ui::nudge("run `wsx remote check` once the server's set up for full validation");
    } else {
        crate::ui::nudge("edit .wsx.json / .wsx-hooks/, then `wsx claude`");
    }
    Ok(())
}

fn refresh_schema_only(root: &Path) -> Result<()> {
    crate::ui::header("ws-init", "--refresh-schema");

    let step = crate::ui::step("cfg", ".wsx.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    crate::config::write_json_pretty(&root.join(".wsx.schema.json"), &schema)?;
    step.ok();
    crate::ui::summary_ok(&[format!("{} refreshed", root.join(".wsx.schema.json").display())]);
    Ok(())
}

/// Nearest ancestor of `root` holding `.wsx.json`, or `None` if not nested inside a workspace.
fn enclosing_workspace(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .skip(1)
        .find(|dir| dir.join(".wsx.json").is_file())
        .map(Path::to_path_buf)
}

/// Build a repo entry, best-effort scaffolding the auto-derived `services` block so routing is visible in `.wsx.json`; `wsx up` re-derives live regardless.
fn scaffold_repo(root: &Path, name: &str, runtime: &str) -> RepoEntry {
    let repo_dir = root.join(name);
    // No server exists yet — the workspace is being created — so derivation runs on the local site.
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
    let mut names = Vec::new();
    // Shared walk skips symlinks/non-dirs and swallows per-entry errors; only the dotfile skip and `is_git_repo` filter are init-specific.
    for (name, path) in crate::workspace::list_child_dirs(root) {
        let Some(name) = name else {
            // Non-UTF-8 dir names would round-trip lossily through `.wsx.json` and break later worktree lookups.
            crate::ui::note(&format!(
                "skipping non-UTF-8 directory name: {}",
                path.display()
            ));
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if crate::workspace::is_git_repo(&path) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Scaffold a starter shell script per repo per event under `.wsx-hooks/`, as
/// `<repo>-<event>.sh`. They're inert until a repo's `scripts` block names one —
/// `wsx` runs only what `.wsx.json` declares — so the template body says how.
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
         # {repo}'s {event} script. Runs from the worktree; its path is $WSX_WORKTREE.\n\
         # Enable it by adding to {repo}'s `scripts` in .wsx.json:\n\
         #   \"{event}\": \".wsx-hooks/{name}\"\n\
         set -euo pipefail\n\
         \n\
         echo \"{repo}-{event}: worktree=$WSX_WORKTREE\"\n"
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
