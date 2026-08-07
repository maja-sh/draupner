use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{RemoteConfig, RepoConfig, RepoEntry, WsConfig};
use crate::compose;

const HOOK_EVENTS: &[&str] = &["post-create", "up", "down", "post-remove", "status", "test"];

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
                "no `.ws.json` in {} or any parent — `ws init --refresh-schema` \
                 only runs inside an initialized workspace.",
                cwd.display()
            )))?;
        return refresh_schema_only(&root);
    }

    let remote_cfg = build_remote(&cwd, remote, remote_path)?;
    init_local(&cwd, remote_cfg, domain, runtime, no_verify)
}

/// Turn the `--remote[=host]` / `--remote-path` flags into a `RemoteConfig`, falling back to `$WS_REMOTE_HOST` then the stored global default; `None` when `--remote` wasn't given.
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

    // Only an explicit `--runtime` is persisted; a detected one stays out of `.ws.json` so the workspace re-detects wherever it's opened.
    let effective =
        runtime.clone().unwrap_or_else(|| crate::config::detect_runtime().unwrap_or("podman").to_string());

    // Blocks *creating* a nested root (not running `ws` from a subdir), because an outer `ws clean`/`rm` would tear an inner workspace down.
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

    if root.join(".ws.json").exists() {
        bail!(
            "{} already exists. Remove it (or use `--refresh-schema` to regenerate the JSON Schema).",
            root.join(".ws.json").display()
        );
    }

    // Verify before writing anything so a strict-mode refusal leaves no half-written config behind.
    if let Some(r) = &remote {
        super::verify_remote_client(r, no_verify)?;
    }

    let repos = detect_repos(root)?;

    let remote_note = remote.as_ref().map(|r| r.connection.clone());
    let pinned = runtime.is_some();
    let step_config = crate::ui::step("cfg", ".ws.json");
    let cfg = WsConfig {
        schema: Some("./.ws.schema.json".to_string()),
        domain,
        runtime,
        remote,
        repos: repos.iter().map(|r| scaffold_repo(root, r, &effective)).collect(),
    };
    let cfg_path = root.join(".ws.json");
    crate::config::write_json_pretty(&cfg_path, &cfg)?;
    step_config.ok();

    let step_schema = crate::ui::step("cfg", ".ws.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    crate::config::write_json_pretty(&root.join(".ws.schema.json"), &schema)?;
    step_schema.ok();

    let step_hooks = crate::ui::step("hks", ".ws-hooks/ templates");
    write_hook_templates(&root.join(".ws-hooks"), &repos)?;
    step_hooks.ok();

    let mut summary = Vec::new();
    summary.push(format!(".ws.json  ({} repos: {})", repos.len(), repos.join(", ")));
    if let Some(conn) = &remote_note {
        summary.push(format!("remote  → {conn}"));
    }
    // A detected runtime isn't written to `.ws.json`, so surface it here or it'd be invisible until something used it.
    summary.push(if pinned {
        format!("runtime → {effective} (pinned in .ws.json)")
    } else {
        format!("runtime → {effective} (detected — add `runtime` to .ws.json to pin it)")
    });
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

    let step = crate::ui::step("cfg", ".ws.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    crate::config::write_json_pretty(&root.join(".ws.schema.json"), &schema)?;
    step.ok();
    crate::ui::summary_ok(&[format!("{} refreshed", root.join(".ws.schema.json").display())]);
    Ok(())
}

/// Nearest ancestor of `root` holding `.ws.json`, or `None` if not nested inside a workspace.
fn enclosing_workspace(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .skip(1)
        .find(|dir| dir.join(".ws.json").is_file())
        .map(Path::to_path_buf)
}

/// Build a repo entry, best-effort scaffolding the auto-derived `services` block so routing is visible in `.ws.json`; `ws up` re-derives live regardless.
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
    // One broken entry (e.g. dangling symlink) shouldn't abort the whole scan.
    for entry in fs::read_dir(root).context("read cwd")?.filter_map(Result::ok) {
        let path = entry.path();
        // Symlinks can point outside the workspace and confuse later ops.
        let Ok(meta) = entry.file_type() else { continue };
        if meta.is_symlink() || !meta.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            // Non-UTF-8 names would round-trip lossily through `.ws.json` and break later worktree lookups.
            tracing::warn!(
                path = %entry.path().display(),
                "skipping non-UTF-8 directory name in workspace scan"
            );
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

fn write_hook_templates(dir: &Path, repos: &[String]) -> Result<()> {
    fs::create_dir_all(dir)?;
    for repo in repos {
        let repo_dir = dir.join(repo);
        fs::create_dir_all(&repo_dir)?;
        for event in HOOK_EVENTS {
            let path = repo_dir.join(event);
            if !path.exists() {
                fs::write(&path, hook_template(repo, event))?;
            }
        }
    }
    Ok(())
}

fn hook_template(repo: &str, event: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         # {repo}/{event} — runs from workspace root, worktree path is $1.\n\
         # `chmod +x` this file to enable it; ws skips non-executable hooks.\n\
         set -euo pipefail\n\
         \n\
         echo \"{repo}-{event}: worktree=$1\"\n"
    )
}
