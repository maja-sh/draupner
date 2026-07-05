use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{RepoEntry, WsConfig};

const HOOK_EVENTS: &[&str] = &["post-create", "up", "down", "post-remove", "status", "test"];

pub fn run(refresh_schema: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;

    if refresh_schema {
        return refresh_schema_only(&cwd);
    }

    init_local(&cwd)
}

fn init_local(root: &Path) -> Result<()> {
    crate::ui::header("ws-init", &format!("in {}", root.display()));

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

    if root.join(".ws.json").exists() {
        bail!(
            "{} already exists. Remove it (or use `--refresh-schema` to regenerate the JSON Schema).",
            root.join(".ws.json").display()
        );
    }

    let repos = detect_repos(root)?;

    let step_config = crate::ui::step("cfg", ".ws.json");
    let cfg = WsConfig {
        schema: Some("./.ws.schema.json".to_string()),
        repos: repos.iter().map(|r| RepoEntry::Name(r.clone())).collect(),
    };
    let cfg_path = root.join(".ws.json");
    write_json_pretty(&cfg_path, &cfg)
        .with_context(|| format!("write {}", cfg_path.display()))?;
    step_config.ok();

    let step_schema = crate::ui::step("cfg", ".ws.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    write_json_pretty(&root.join(".ws.schema.json"), &schema)?;
    step_schema.ok();

    let step_hooks = crate::ui::step("hks", ".ws-hooks/ templates");
    write_hook_templates(&root.join(".ws-hooks"), &repos)?;
    step_hooks.ok();

    let mut summary = Vec::new();
    summary.push(format!(".ws.json  ({} repos: {})", repos.len(), repos.join(", ")));
    summary.push(".ws.schema.json".to_string());
    summary.push(".ws-hooks/".to_string());
    crate::ui::summary_ok(&summary);
    crate::ui::nudge("edit .ws.json / .ws-hooks/, then `ws claude`");
    Ok(())
}

fn refresh_schema_only(root: &Path) -> Result<()> {
    crate::ui::header("ws-init", "--refresh-schema");

    // Refuse to scatter an orphan schema in a non-workspace dir.
    if !root.join(".ws.json").exists() {
        bail!(
            "no {} here — `ws init --refresh-schema` only runs inside an initialized workspace.",
            root.join(".ws.json").display()
        );
    }

    let step = crate::ui::step("cfg", ".ws.schema.json");
    let schema = schemars::schema_for!(WsConfig);
    write_json_pretty(&root.join(".ws.schema.json"), &schema)?;
    step.ok();
    crate::ui::summary_ok(&[format!("{} refreshed", root.join(".ws.schema.json").display())]);
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

fn detect_repos(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    // Swallow per-entry read errors — one broken symlink shouldn't abort the
    // whole scan. Errors on the top-level `read_dir` still propagate.
    for entry in fs::read_dir(root).context("read cwd")?.filter_map(Result::ok) {
        let path = entry.path();
        // Skip symlinks (they can point outside the workspace and confuse
        // later ops). Also skip anything that isn't a real dir.
        let Ok(meta) = entry.file_type() else { continue };
        if meta.is_symlink() || !meta.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            // Non-UTF-8 dir names would round-trip through `.ws.json` lossily
            // and break later worktree lookups. Skip with a warn instead of
            // silently corrupting the config.
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

fn write_json_pretty<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{s}\n"))?;
    Ok(())
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
