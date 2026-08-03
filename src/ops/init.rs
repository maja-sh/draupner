use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{RemoteConfig, RepoConfig, WsConfig};
use crate::compose;

/// The lifecycle events `ws` dispatches, in the order they happen.
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
        // Scaffolding is rightly cwd-bound, but refreshing an existing workspace is
        // the "which workspace am I in" question every other op answers by walking up.
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

/// `workspace_path` is written explicitly, rather than left to default, so the
/// server layout is visible in the scaffolded `.ws.json`.
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

    // Blocks *creating* a nested root, not running `ws` from a subdir: under the
    // outer's `.worktrees/`, an outer `ws clean`/`rm` would tear this one down.
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

    // Before writing anything, so a strict-mode refusal leaves no half-written config.
    if let Some(r) = &remote {
        super::verify_remote_client(r, no_verify)?;
    }

    let repos = detect_repos(root)?;

    let remote_note = remote.as_ref().map(|r| r.connection.clone());
    let cfg = WsConfig {
        schema: Some("./.ws.schema.json".to_string()),
        domain,
        // Written explicitly, like workspace_path, so it's discoverable in .ws.json.
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

    write_schema(root)?;
    crate::ui::summary_ok(&[format!("{} refreshed", root.join(".ws.schema.json").display())]);
    Ok(())
}

/// Shared by `ws init` and `--refresh-schema`, step UI included, so the two
/// can't drift on the filename or the step label.
fn write_schema(root: &Path) -> Result<()> {
    let step = crate::ui::step("cfg", ".ws.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    crate::config::write_json_pretty(&root.join(".ws.schema.json"), &schema)?;
    step.ok();
    Ok(())
}

fn enclosing_workspace(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .skip(1)
        .find(|dir| dir.join(".ws.json").is_file())
        .map(Path::to_path_buf)
}

/// Scaffolds the derived `services` block so the routing is visible in `.ws.json`
/// from day one. Best-effort: with no compose file or no runtime to run `compose
/// config`, it stays a bare-name entry and `ws up` re-derives live regardless.
fn scaffold_repo(root: &Path, name: &str, runtime: &str) -> RepoConfig {
    let repo_dir = root.join(name);
    // The workspace is being created, so there is no server yet to derive on.
    let services = compose::has_compose_file(&repo_dir)
        .then(|| {
            compose::derive(&crate::remote::Site::Local, runtime, &repo_dir.to_string_lossy()).ok().map(|d| d.services)
        })
        .flatten()
        .filter(|s| !s.is_empty());
    // With nothing derived this is all defaults, which serializes back to the
    // bare-name spelling.
    RepoConfig { name: name.to_string(), services: services.unwrap_or_default(), ..Default::default() }
}

fn detect_repos(root: &Path) -> Result<Vec<String>> {
    // `list_child_dirs` swallows a failing `read_dir` and returns an empty vec,
    // which here would scaffold a repo-less `.ws.json` off an unreadable cwd
    // instead of saying so. Probe first, then share the walk.
    fs::read_dir(root).context("read cwd")?;

    let mut names = Vec::new();
    for (name, path) in crate::workspace::list_child_dirs(root) {
        let Some(name) = name else {
            // A non-UTF-8 name would round-trip through `.ws.json` lossily and
            // break later worktree lookups, so it's skipped rather than written.
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
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// The scaffolded scripts stay inert until a repo's `scripts` block names one, so
/// the template body says how. Written executable: `scripts` invokes the file.
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
