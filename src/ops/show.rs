//! Focused per-worktree endpoint: everything you need to operate one task's
//! worktree, one call. Narrower than `drup list` (single branch, no siblings),
//! richer than `drup path` (services, ports, hostnames, exec syntax on top of
//! paths). The `--json` shape doubles as the `task.repos[]` payload of
//! `drup context`, which is why field names are self-documenting — the LLM
//! endpoint leans on names in place of a schema round-trip.

use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, ServiceConfig, WsConfig};
use crate::hooks;
use crate::inject;
use crate::remote::Site;
use crate::workspace;

/// Container liveness runs the same `status` hook `drup list` uses; cap it short so a hung stack can't stall the summary.
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);

pub fn run(branch: Option<&str>, json: bool, describe: bool) -> Result<()> {
    if describe {
        print!("{}", SCHEMA_TEXT);
        return Ok(());
    }

    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let branch = resolve_branch(branch, &ws.root, &cfg)?;
    let site = Site::resolve(&cfg).ok();

    let data = collect(&cfg, &ws.root, &branch, site.as_ref());

    if data.repos.is_empty() {
        return Err(super::preflight(format!(
            "no worktrees for {branch}; run `drup new` first"
        )));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else {
        render_human(&data);
    }
    Ok(())
}

/// Explicit branch wins; otherwise walk out of the CWD to the enclosing task dir and recover the *original* branch name from git (not the sanitized dir name), so downstream `drup exec`s see the branch the user typed.
fn resolve_branch(branch: Option<&str>, root: &Path, cfg: &WsConfig) -> Result<String> {
    if let Some(b) = branch {
        return Ok(b.to_string());
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let worktrees_root = root.join(".worktrees");
    let worktrees_root = worktrees_root.canonicalize().unwrap_or(worktrees_root);
    let (task, cwd_repo) = workspace::task_repo_under(&cwd, &worktrees_root).ok_or_else(|| {
        super::preflight(
            "drup show without a branch must be run from inside a worktree; \
             pass `drup show <branch>` to name one explicitly",
        )
    })?;
    // Probe git for just the repo whose worktree we're inside — sibling repos would give the same branch anyway (a task groups worktrees under one branch), and one `git worktree list` is cheaper than one per configured repo.
    let target_dir = worktrees_root.join(&task);
    let target_canon = target_dir.canonicalize().unwrap_or(target_dir);
    let probe_repo = |repo: &str| -> Option<String> {
        crate::git::worktree_list(&root.join(repo))
            .into_iter()
            .find(|(p, _)| p.canonicalize().unwrap_or_else(|_| p.clone()) == target_canon.join(repo))
            .and_then(|(_, branch)| branch)
    };
    if let Some(repo) = cwd_repo.as_deref() {
        if let Some(b) = probe_repo(repo) {
            return Ok(b);
        }
    }
    for entry in &cfg.repos {
        if let Some(b) = probe_repo(entry.name()) {
            return Ok(b);
        }
    }
    Ok(task)
}

#[derive(Serialize)]
pub(crate) struct ShowData {
    pub branch: String,
    pub workspace_root: PathBuf,
    pub remote: Option<RemoteInfo>,
    pub repos: Vec<RepoInfo>,
}

#[derive(Serialize, Clone)]
pub(crate) struct RemoteInfo {
    pub connection: String,
    pub worktree_base_absolute: String,
}

#[derive(Serialize)]
pub(crate) struct RepoInfo {
    pub name: String,
    pub kind: String,
    pub worktree_absolute: PathBuf,
    pub is_running: Option<bool>,
    pub services: Vec<ServiceInfo>,
    pub log_path_absolute: Option<PathBuf>,
    pub exec_command_prefix: String,
}

#[derive(Serialize)]
pub(crate) struct ServiceInfo {
    pub name: String,
    pub allocated_host_port: Option<u16>,
    pub routed_hostname: Option<String>,
    pub is_http_routed: bool,
}

pub(crate) fn collect(cfg: &WsConfig, root: &Path, branch: &str, site: Option<&Site>) -> ShowData {
    let sanitized = workspace::sanitize_branch(branch);

    let remote = cfg.remote.as_ref().map(|r| RemoteInfo {
        connection: r.connection.clone(),
        worktree_base_absolute: match site {
            Some(s) => s.abs_base(&r.worktree_base(root)),
            None => r.worktree_base(root),
        },
    });

    let mut repos = Vec::new();
    for entry in &cfg.repos {
        let name = entry.name();
        let Some(worktree_absolute) = workspace::worktree_for(root, branch, name) else {
            continue;
        };

        let is_host = cfg.is_host_repo(name);
        let kind = if cfg.is_container_repo(name, &worktree_absolute) {
            "container"
        } else {
            "host"
        };

        // One `.drup-meta` read serves both liveness (never-up short-circuit) and the service list.
        let (port_map, recorded_services) = crate::ports::load_meta(root, &sanitized, name);

        let is_running = liveness(
            cfg, root, &worktree_absolute, branch, &sanitized, name, is_host, site, &recorded_services,
        );
        let services = services_for(cfg, &sanitized, name, port_map, recorded_services);

        let log_path_absolute = is_host
            .then(|| site.and_then(|s| crate::supervise::log_path(s, &crate::supervise::proc_name(branch, name))))
            .flatten();
        let exec_command_prefix = format!("drup exec --branch {branch} {name} --");

        repos.push(RepoInfo {
            name: name.to_string(),
            kind: kind.to_string(),
            worktree_absolute,
            is_running,
            services,
            log_path_absolute,
            exec_command_prefix,
        });
    }

    ShowData {
        branch: branch.to_string(),
        workspace_root: root.to_path_buf(),
        remote,
        repos,
    }
}

/// `None` when the site is unreachable — the caller shouldn't guess.
#[allow(clippy::too_many_arguments)]
fn liveness(
    cfg: &WsConfig,
    root: &Path,
    worktree: &Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    is_host: bool,
    site: Option<&Site>,
    recorded_services: &BTreeMap<String, ServiceConfig>,
) -> Option<bool> {
    let site = site?;
    if is_host {
        let name = crate::supervise::proc_name(branch, repo);
        return Some(crate::supervise::is_active(site, &name));
    }
    // Never-up short-circuit — port_map alone would misread an HTTP-only container, which allocates no ports (`inject::prepare` filters on `!is_http`) but still records services once `up` runs. Recorded services is the honest sentinel for "has ever been up".
    if recorded_services.is_empty() {
        return Some(false);
    }
    let is_container = cfg.is_container_repo(repo, worktree);
    let cmd = match hooks::resolve(cfg, root, repo, "status", is_container) {
        // `status` can never be interactive (refused at config load); .cmd only.
        Ok(Some(s)) => s.cmd,
        Ok(None) => return None,
        Err(e) => {
            crate::ui::note(&format!("{repo}: {e}"));
            return None;
        }
    };
    let extra_env = inject::prepare(
        cfg, site, root, worktree, sanitized, repo, inject::Phase::Inspect,
    )
    .ok()?;
    // Remote workspace container status must run on the server, otherwise the local runtime answers a question about the server's containers — the exact mismatch `drup list` avoids at list.rs:47-64.
    let out = match site {
        Site::Local => hooks::capture_command(
            root, worktree, repo, sanitized, "status", &cmd, STATUS_TIMEOUT, &extra_env,
        )?,
        Site::Remote { connection, .. } => {
            let r = cfg.remote.as_ref()?;
            let base = site.abs_base(&r.worktree_base(root));
            crate::remote::capture_status(
                connection, &base, sanitized, repo, &extra_env, &cmd, STATUS_TIMEOUT.as_secs(),
            )?
        }
    };
    Some(match crate::ui::classify_status(&out) {
        Some(crate::ui::StatusKind::Healthy | crate::ui::StatusKind::Degraded) => true,
        Some(crate::ui::StatusKind::Stopped | crate::ui::StatusKind::Crashed) => false,
        None => out.trim() != "0",
    })
}

/// Services with their allocated port + routing status. `recorded` comes from the `.drup-meta` last written at `up` (which covers auto-derived compose services); fall back to the `.drup.json`-declared set for a not-yet-up worktree.
pub(crate) fn services_for(
    cfg: &WsConfig,
    sanitized: &str,
    repo: &str,
    port_map: BTreeMap<String, u16>,
    recorded: BTreeMap<String, ServiceConfig>,
) -> Vec<ServiceInfo> {
    let declared: BTreeMap<String, ServiceConfig> = cfg
        .repo_config(repo)
        .map(|rc| rc.services.clone())
        .unwrap_or_default();
    let services = if recorded.is_empty() { declared } else { recorded };
    let domain = cfg.domain.as_deref();
    let routing_on = cfg.routing_active() && domain.is_some();
    services
        .into_iter()
        .map(|(svc_name, svc)| {
            let routed_hostname = svc
                .subdomain(&svc_name)
                .filter(|_| routing_on)
                .map(|s| inject::ws_host(sanitized, s, domain.unwrap()));
            ServiceInfo {
                allocated_host_port: port_map.get(&svc_name).copied(),
                is_http_routed: routed_hostname.is_some(),
                name: svc_name,
                routed_hostname,
            }
        })
        .collect()
}

fn render_human(d: &ShowData) {
    crate::ui::header("drup-show", &d.branch);
    println!();
    println!("  workspace  {}", d.workspace_root.display());
    match &d.remote {
        Some(r) => println!("  remote     {}", r.connection),
        None => println!("  remote     —"),
    }
    for repo in &d.repos {
        let state = match repo.is_running {
            Some(true) => "running",
            Some(false) => "stopped",
            None => "unknown",
        };
        println!();
        println!("  {}  ({}, {})", repo.name, repo.kind, state);
        // Pad labels to the widest one in the block. "exec" is always present at 4 chars, so it wins over "log" (3) — service names are the only variable.
        let label_width = repo.services.iter().map(|s| s.name.len()).max().unwrap_or(0).max(4);
        for svc in &repo.services {
            let name = &svc.name;
            match (&svc.routed_hostname, svc.allocated_host_port) {
                (Some(host), Some(port)) => println!("    {name:label_width$}  https://{host}  (:{port})"),
                (Some(host), None) => println!("    {name:label_width$}  https://{host}  (unallocated)"),
                (None, Some(port)) => println!("    {name:label_width$}  :{port}"),
                (None, None) => println!("    {name:label_width$}  (unallocated)"),
            }
        }
        if let Some(log) = &repo.log_path_absolute {
            println!("    {:label_width$}  {}", "log", log.display());
        }
        println!(
            "    {:label_width$}  {} <cmd>",
            "exec", repo.exec_command_prefix
        );
    }
}

/// The `--describe` schema block, kept next to the emitting code so drift is obvious in review. Any change to `ShowData`'s field set MUST update this text in the same commit; the LLM skill leans on it as ground truth.
const SCHEMA_TEXT: &str = r#"drup show — per-worktree status endpoint.

Human by default. `--json` emits the same data as a structured object:

{
  "branch":              original branch name (may contain "/"),
  "workspace_root":      absolute path to the workspace root,
  "remote":              null (local workspace) or:
                           { "connection": "<ssh-host>",
                             "worktree_base_absolute": "<server-side path>" },
  "repos": [
    {
      "name":                  repo name from .drup.json,
      "kind":                  "host" | "container",
      "worktree_absolute":     absolute path to this repo's worktree,
      "is_running":            true | false | null,
      "services": [
        {
          "name":                  service name,
          "allocated_host_port":   integer or null,
          "routed_hostname":       HTTP hostname or null,
          "is_http_routed":        true if reachable via https://<routed_hostname>
        }
      ],
      "log_path_absolute":     absolute path to the host log, null for containers,
      "exec_command_prefix":   "drup exec --branch <branch> <repo> --" — append your command
    }
  ]
}

Semantics tooling should ground on:
- `is_running` is a live probe. Host repos use drup's supervise::is_active check;
  container repos run their `status` hook capped at 3s. `null` means the site
  is out of reach or drup couldn't decide.
- `allocated_host_port` is null on a worktree that has never been up; every
  other state has a port.
- `routed_hostname` is the correct answer for HTTPS access. Do NOT re-derive
  it from branch/repo names — drup applied its own sanitization.
- `exec_command_prefix` ends in ` --`; append a space and your command.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal workspace on disk that `collect` can read against — a `.drup.json`, the target repo as a bare git dir, a `.worktrees/<task>/<repo>` dir, and an optional `.drup-meta/*.json` port record.
    fn scaffold_workspace(root: &Path, ws_json: &str, sanitized_task: &str, repo: &str, meta: Option<&str>) {
        std::fs::write(root.join(".drup.json"), ws_json).expect("write .drup.json");
        // Bare git repo at <root>/<repo> — worktrees::worktrees() skips repos where is_git_repo is false.
        let repo_dir = root.join(repo);
        std::fs::create_dir_all(repo_dir.join(".git")).expect("mkdir repo/.git");
        std::fs::write(repo_dir.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        // The worktree dir the caller can see via workspace::worktree_for.
        std::fs::create_dir_all(root.join(".worktrees").join(sanitized_task).join(repo))
            .expect("mkdir worktree");
        if let Some(m) = meta {
            let meta_dir = root.join(".drup-meta");
            std::fs::create_dir_all(&meta_dir).expect("mkdir .drup-meta");
            let fname = crate::ports::override_filename(sanitized_task, repo)
                .replace(".override.yml", ".json");
            std::fs::write(meta_dir.join(fname), m).expect("write meta");
        }
    }

    #[test]
    fn declared_services_appear_before_first_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        scaffold_workspace(
            root,
            r#"{"domain":"example.test","repos":[{"name":"api","kind":"host","services":{"web":{"http":true,"port":8080}}}]}"#,
            "feat-x",
            "api",
            None,
        );
        let cfg = config::load(root).unwrap();
        let data = collect(&cfg, root, "feat/x", None);
        assert_eq!(data.branch, "feat/x");
        assert_eq!(data.repos.len(), 1);
        let repo = &data.repos[0];
        assert_eq!(repo.name, "api");
        assert_eq!(repo.kind, "host");
        assert_eq!(repo.services.len(), 1);
        let svc = &repo.services[0];
        assert_eq!(svc.name, "web");
        assert!(svc.is_http_routed, "http:true + domain set ⇒ routed");
        assert_eq!(svc.routed_hostname.as_deref(), Some("feat-x.web.example.test"));
        assert!(svc.allocated_host_port.is_none(), "no meta yet ⇒ no allocated port");
    }

    #[test]
    fn allocated_ports_from_meta_win_over_declared() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        scaffold_workspace(
            root,
            r#"{"domain":"example.test","repos":[{"name":"api","kind":"host","services":{"web":{"http":true,"port":8080}}}]}"#,
            "feat-x",
            "api",
            Some(r#"{"ports":{"web":20003},"services":{"web":{"http":true,"port":8080}}}"#),
        );
        let cfg = config::load(root).unwrap();
        let data = collect(&cfg, root, "feat/x", None);
        let svc = &data.repos[0].services[0];
        assert_eq!(svc.allocated_host_port, Some(20003));
        assert_eq!(svc.routed_hostname.as_deref(), Some("feat-x.web.example.test"));
    }

    #[test]
    fn raw_tcp_service_is_not_http_routed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        scaffold_workspace(
            root,
            r#"{"domain":"example.test","repos":[{"name":"agent","kind":"host","services":{"sim_api":{"port":4840},"sim_opc":{"port":5555}}}]}"#,
            "feat-x",
            "agent",
            None,
        );
        let cfg = config::load(root).unwrap();
        let data = collect(&cfg, root, "feat/x", None);
        let repo = &data.repos[0];
        for svc in &repo.services {
            assert!(!svc.is_http_routed, "{}: raw-TCP service must not appear routed", svc.name);
            assert!(svc.routed_hostname.is_none(), "{}: no hostname for raw-TCP", svc.name);
        }
    }

    #[test]
    fn exec_prefix_and_log_path_are_shaped_for_the_llm() {
        // Passing site=Some(&Site::Local) exercises the log-path branch without mutating `$HOME` — cargo runs tests in parallel and env::set_var races other tests reading HOME.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        scaffold_workspace(
            root,
            r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":8080}}}]}"#,
            "feat-x",
            "api",
            None,
        );
        let cfg = config::load(root).unwrap();
        let data = collect(&cfg, root, "feat/x", Some(&Site::Local));
        let repo = &data.repos[0];
        assert_eq!(repo.exec_command_prefix, "drup exec --branch feat/x api --");
        // Whatever HOME points at, the suffix is what the LLM appends to its own log-tail commands.
        let log = repo
            .log_path_absolute
            .as_ref()
            .and_then(|p| p.to_str())
            .expect("host repo carries a log path");
        assert!(
            log.ends_with("/.drup-run/drup-feat-x-api.log"),
            "log path should follow the ~/.drup-run/<proc_name>.log convention, got {log}"
        );
    }

    #[test]
    fn log_path_for_a_remote_workspace_names_the_server_home_not_ours() {
        let site = Site::Remote { connection: "srv".into(), home: "/home/server-user".into() };
        let got = crate::supervise::log_path(&site, "drup-feat-x-api")
            .and_then(|p| p.to_str().map(str::to_owned))
            .expect("remote log path");
        assert_eq!(got, "/home/server-user/.drup-run/drup-feat-x-api.log");
    }

    #[test]
    fn describe_prints_and_exits_ok() {
        run(None, false, true).expect("--describe never fails");
    }

    #[test]
    fn schema_text_mentions_every_json_field() {
        // Drift alarm: anchor on the `"<field>":` JSON key form so short field names ("name", "kind", "remote") don't false-pass against substrings inside longer identifiers ("hostname", "routed_hostname", "worktree_absolute"). Adding a field without documenting it now trips the assertion.
        for field in [
            "branch",
            "workspace_root",
            "remote",
            "repos",
            "name",
            "kind",
            "worktree_absolute",
            "is_running",
            "services",
            "allocated_host_port",
            "routed_hostname",
            "is_http_routed",
            "log_path_absolute",
            "exec_command_prefix",
        ] {
            let needle = format!("\"{field}\":");
            assert!(
                SCHEMA_TEXT.contains(&needle),
                "SCHEMA_TEXT is missing `{needle}` — describe output has drifted from the actual JSON shape"
            );
        }
    }
}
