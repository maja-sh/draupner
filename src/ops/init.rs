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
        remote: None,
        repos: repos.iter().map(|r| RepoEntry::Name(r.clone())).collect(),
    };
    let cfg_path = root.join(".ws.json");
    write_json_pretty(&cfg_path, &cfg)
        .with_context(|| format!("write {}", cfg_path.display()))?;
    step_config.ok();

    let step_schema = crate::ui::step("cfg", ".ws.schema.json");
    write_schema(&root.join(".ws.schema.json"))?;
    step_schema.ok();

    let step_hooks = crate::ui::step("hks", ".ws-hooks/ templates");
    write_hook_templates(&root.join(".ws-hooks"), &repos)?;
    step_hooks.ok();

    let mut summary = Vec::new();
    summary.push(format!(".ws.json  ({} repos: {})", repos.len(), repos.join(", ")));
    summary.push(".ws.schema.json".to_string());
    summary.push(".ws-hooks/".to_string());
    crate::ui::summary_ok(&summary);
    crate::ui::nudge("edit .ws.json / .ws-hooks/, then `ws register`");
    Ok(())
}

fn refresh_schema_only(root: &Path) -> Result<()> {
    crate::ui::header("ws-init", "--refresh-schema");

    let step = crate::ui::step("cfg", ".ws.schema.json");
    write_schema(&root.join(".ws.schema.json"))?;
    step.ok();
    crate::ui::summary_ok(&[format!("{} refreshed", root.join(".ws.schema.json").display())]);
    Ok(())
}

fn detect_repos(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root).context("read cwd")? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
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

fn write_json_pretty<T: serde::Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{s}\n"))?;
    Ok(())
}

fn write_schema(path: &Path) -> Result<()> {
    let schema = schemars::schema_for!(WsConfig);
    let s = serde_json::to_string_pretty(&schema)?;
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
