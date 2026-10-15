//! Cwd-adaptive JSON wakeup for LLM agents.

use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::config::{self, WsConfig};
use crate::ops::show;
use crate::remote::Site;
use crate::workspace;

pub fn run(describe: bool) -> Result<()> {
    if describe {
        print!("{}", SCHEMA_TEXT);
        return Ok(());
    }

    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let site = Site::resolve(&cfg).ok();
    let cwd = std::env::current_dir()?;

    let ctx = collect(&cfg, &ws.root, &cwd, site.as_ref());
    println!("{}", serde_json::to_string_pretty(&ctx)?);
    Ok(())
}

#[derive(Serialize)]
struct Context {
    drup_version: &'static str,
    workspace: Workspace,
    current: Current,
    /// `null` when the cwd isn't inside a task worktree — the agent has workspace-level facts but no per-task detail.
    task: Option<Task>,
    hints: Vec<&'static str>,
}

#[derive(Serialize)]
struct Workspace {
    root: PathBuf,
    domain: Option<String>,
    runtime: String,
    remote: Option<show::RemoteInfo>,
}

#[derive(Serialize)]
struct Current {
    cwd: PathBuf,
    /// The branch (as originally typed by the user) whose worktree the cwd sits in; `null` when cwd is outside `.worktrees/`.
    task: Option<String>,
    /// The repo whose worktree subtree the cwd sits in; `null` when cwd is at the task root but not in any repo.
    repo: Option<String>,
    task_dir: Option<PathBuf>,
    repo_worktree: Option<PathBuf>,
}

#[derive(Serialize)]
struct Task {
    branch: String,
    branch_sanitized: String,
    state: TaskState,
    repos: Vec<show::RepoInfo>,
}

/// Aggregated per-task health; the label is a shortcut, the numerics are ground truth.
#[derive(Serialize)]
struct TaskState {
    kind: &'static str,
    healthy_repos: usize,
    total_repos: usize,
    any_running: bool,
}

fn collect(cfg: &WsConfig, root: &Path, cwd: &Path, site: Option<&Site>) -> Context {
    let current = build_current(cfg, root, cwd);
    let task = current.task.as_deref().map(|branch| build_task(cfg, root, branch, site));
    let workspace = build_workspace(cfg, root, site);
    let hints = agent_hints(&workspace, &current, task.as_ref());
    Context {
        drup_version: env!("CARGO_PKG_VERSION"),
        workspace,
        current,
        task,
        hints,
    }
}

fn build_workspace(cfg: &WsConfig, root: &Path, site: Option<&Site>) -> Workspace {
    let remote = cfg.remote.as_ref().map(|r| show::RemoteInfo {
        connection: r.connection.clone(),
        worktree_base_absolute: match site {
            Some(s) => s.abs_base(&r.worktree_base(root)),
            None => r.worktree_base(root),
        },
    });
    Workspace {
        root: root.to_path_buf(),
        domain: cfg.domain.clone(),
        runtime: cfg.runtime().to_string(),
        remote,
    }
}

/// cwd → (task, repo) inference plus original-branch recovery via git.
fn build_current(cfg: &WsConfig, root: &Path, cwd: &Path) -> Current {
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let worktrees_root = root.join(".worktrees");
    let worktrees_root_canon = worktrees_root.canonicalize().unwrap_or(worktrees_root);

    let (task_dir_name, repo_name) = match workspace::task_repo_under(&cwd_canon, &worktrees_root_canon) {
        Some((t, r)) => (Some(t), r),
        None => (None, None),
    };

    let branch = task_dir_name.as_deref().and_then(|dir| recover_branch(cfg, root, &worktrees_root_canon, dir));
    let task_dir = task_dir_name.as_ref().map(|d| worktrees_root_canon.join(d));
    let repo_worktree = match (task_dir.as_ref(), repo_name.as_deref()) {
        (Some(td), Some(r)) => Some(td.join(r)),
        _ => None,
    };

    Current { cwd: cwd_canon, task: branch, repo: repo_name, task_dir, repo_worktree }
}

/// Recover the user-typed branch from a sanitized task-dir name via git; None when no repo confirms it, since the sanitized name is not a valid substitute for downstream `drup exec`.
fn recover_branch(cfg: &WsConfig, root: &Path, worktrees_root: &Path, task_dir: &str) -> Option<String> {
    let target = worktrees_root.join(task_dir);
    let target = target.canonicalize().unwrap_or(target);
    for entry in &cfg.repos {
        let repo = entry.name();
        let probe = crate::git::worktree_list(&root.join(repo))
            .into_iter()
            .find(|(p, _)| p.canonicalize().unwrap_or_else(|_| p.clone()) == target.join(repo))
            .and_then(|(_, b)| b);
        if probe.is_some() {
            return probe;
        }
    }
    None
}

fn build_task(cfg: &WsConfig, root: &Path, branch: &str, site: Option<&Site>) -> Task {
    let data = show::collect(cfg, root, branch, site);
    let state = aggregate_state(&data.repos);
    Task {
        branch: data.branch,
        branch_sanitized: workspace::sanitize_branch(branch),
        state,
        repos: data.repos,
    }
}

/// Worst-of across repos; unknowns count toward the total but not the numerator.
fn aggregate_state(repos: &[show::RepoInfo]) -> TaskState {
    let total = repos.len();
    let healthy = repos.iter().filter(|r| r.is_running == Some(true)).count();
    let any_unknown = repos.iter().any(|r| r.is_running.is_none());
    let any_running = healthy > 0;

    let kind = if total == 0 {
        "unknown"
    } else if healthy == total {
        "running"
    } else if healthy == 0 && !any_unknown {
        "stopped"
    } else if healthy == 0 && any_unknown {
        "unknown"
    } else {
        "degraded"
    };

    TaskState { kind, healthy_repos: healthy, total_repos: total, any_running }
}

/// Static rules plus conditional ones the current state warrants.
fn agent_hints(workspace: &Workspace, current: &Current, task: Option<&Task>) -> Vec<&'static str> {
    let mut h = vec![
        "task.state.kind is one of: running (all repos healthy) | degraded (some healthy, some not) | stopped (none running) | unknown (state could not be determined). The numeric fields (healthy_repos, total_repos) are the ground truth if the label is unclear.",
        "task.repos[].kind is one of: host (drup-supervised process — drup owns its lifecycle via `drup up`/`down`) | container (compose stack — compose owns the lifecycle).",
        "task.repos[].is_running: true = confirmed running; false = confirmed not running; null = the check could not complete. Do NOT infer 'stopped' from null.",
        "Edit files at absolute paths from task.repos[].worktree_absolute. Do NOT cd into a worktree — operate from wherever the user placed you and use absolute paths.",
        "Run commands inside a repo's worktree via task.repos[].exec_command_prefix — append a space and your command. Direct calls to docker/podman/npm/etc. from outside the prefix miss the injected env and target the wrong stack.",
        "Use services[].routed_hostname verbatim. Do NOT re-derive hostnames from the branch or repo name — drup applied its own sanitization.",
        "services[].allocated_host_port is null on a worktree that has never been brought up. Any other state has a port.",
        "Do NOT auto-run `drup up`, `drup down`, `drup rm`, `drup new`. If a repo isn't running or a worktree doesn't exist, tell the user and let them decide.",
    ];
    if workspace.remote.is_some() {
        h.push("workspace.remote is set — this task's stack lives on the server. `drup exec` is location-transparent (routes through ssh automatically), so keep using it exactly as documented.");
    }
    if current.task.is_none() {
        h.push("cwd is outside any task worktree, so `task` is null. Ask the user which task they want to work in, or ask them to cd into one — do NOT guess.");
    }
    if let Some(t) = task {
        if t.state.total_repos > 0 && !t.state.any_running {
            h.push("This task's stack is not running. Do not attempt to reach any service or read runtime state until the user runs `drup up`.");
        }
    }
    h
}

const SCHEMA_TEXT: &str = r#"drup context — cwd-adaptive JSON wakeup for LLM agents.

Always JSON on stdout. Fields:

{
  "drup_version": string — drup version producing this payload,
  "workspace": {
    "root":     absolute path to the workspace root,
    "domain":   private TLD used for routed hostnames, or null,
    "runtime":  container runtime name (e.g. "podman", "docker"),
    "remote":   null (local workspace) or:
                  { "connection":            ssh host,
                    "worktree_base_absolute": server-side parent dir }
  },
  "current": {
    "cwd":            canonical absolute cwd,
    "task":           branch of the enclosing worktree, or null if cwd is outside .worktrees/,
    "repo":           repo whose subtree cwd sits in, or null,
    "task_dir":       absolute path to the enclosing task dir, or null,
    "repo_worktree":  absolute path to the enclosing repo's worktree, or null
  },
  "task":  null when cwd is outside a task; else:
    {
      "branch":            original branch name (may contain "/"),
      "branch_sanitized":  slash-free form used in filesystem/hostname keys,
      "state": {
        "kind":           "running" | "degraded" | "stopped" | "unknown",
        "healthy_repos":  count of repos with is_running=true,
        "total_repos":    count of repos in the task,
        "any_running":    healthy_repos > 0
      },
      "repos": [ /* worktree_absolute, exec_command_prefix, kind, is_running, services[] */ ]
    },
  "hints": [ /* agent-oriented rules; treat as authoritative */ ]
}

Read `hints` before acting on any other field.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn scaffold(root: &Path, ws_json: &str) {
        std::fs::write(root.join(".drup.json"), ws_json).expect("write .drup.json");
    }

    fn repo(name: &str, is_running: Option<bool>) -> show::RepoInfo {
        show::RepoInfo {
            name: name.to_string(),
            kind: "host".to_string(),
            worktree_absolute: PathBuf::from(format!("/tmp/{name}")),
            is_running,
            services: Vec::new(),
            log_path_absolute: None,
            exec_command_prefix: format!("drup exec --branch feat/x {name} --"),
        }
    }

    #[test]
    fn aggregate_reports_all_healthy_as_running() {
        let s = aggregate_state(&[repo("api", Some(true)), repo("web", Some(true))]);
        assert_eq!(s.kind, "running");
        assert_eq!((s.healthy_repos, s.total_repos), (2, 2));
        assert!(s.any_running);
    }

    #[test]
    fn aggregate_reports_all_stopped_as_stopped() {
        let s = aggregate_state(&[repo("api", Some(false)), repo("web", Some(false))]);
        assert_eq!(s.kind, "stopped");
        assert_eq!((s.healthy_repos, s.total_repos), (0, 2));
        assert!(!s.any_running);
    }

    #[test]
    fn aggregate_reads_mixed_healthy_and_stopped_as_degraded() {
        let s = aggregate_state(&[repo("api", Some(true)), repo("web", Some(false))]);
        assert_eq!(s.kind, "degraded");
        assert_eq!((s.healthy_repos, s.total_repos), (1, 2));
        assert!(s.any_running);
    }

    #[test]
    fn aggregate_treats_all_unknown_as_unknown_not_stopped() {
        // Confusing null for false is exactly what the hint tells the agent not to do — the aggregator must not do it either.
        let s = aggregate_state(&[repo("api", None), repo("web", None)]);
        assert_eq!(s.kind, "unknown");
        assert_eq!((s.healthy_repos, s.total_repos), (0, 2));
        assert!(!s.any_running);
    }

    #[test]
    fn aggregate_treats_mixed_known_and_unknown_as_degraded_when_some_healthy() {
        let s = aggregate_state(&[repo("api", Some(true)), repo("web", None)]);
        assert_eq!(s.kind, "degraded");
        assert_eq!((s.healthy_repos, s.total_repos), (1, 2));
        assert!(s.any_running);
    }

    #[test]
    fn build_current_reports_null_task_when_cwd_is_outside_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        scaffold(root, r#"{"repos":[]}"#);
        let cfg = config::load(root).unwrap();
        let current = build_current(&cfg, root, root);
        assert!(current.task.is_none());
        assert!(current.repo.is_none());
        assert!(current.task_dir.is_none());
        assert!(current.repo_worktree.is_none());
    }

    #[test]
    fn hints_include_the_no_task_warning_when_cwd_is_outside() {
        // If the agent lands with a null task, the loudest hint should be "ask the user, don't guess."
        let workspace = Workspace {
            root: PathBuf::from("/x"),
            domain: None,
            runtime: "podman".to_string(),
            remote: None,
        };
        let current = Current {
            cwd: PathBuf::from("/x"),
            task: None,
            repo: None,
            task_dir: None,
            repo_worktree: None,
        };
        let hints = agent_hints(&workspace, &current, None);
        assert!(
            hints.iter().any(|h| h.contains("cwd is outside any task worktree")),
            "expected no-task hint, got: {hints:?}"
        );
    }

    #[test]
    fn hints_include_the_stack_down_warning_when_no_repo_is_running() {
        let workspace = Workspace {
            root: PathBuf::from("/x"),
            domain: None,
            runtime: "podman".to_string(),
            remote: None,
        };
        let current = Current {
            cwd: PathBuf::from("/x/.worktrees/feat-x"),
            task: Some("feat/x".to_string()),
            repo: None,
            task_dir: Some(PathBuf::from("/x/.worktrees/feat-x")),
            repo_worktree: None,
        };
        let task = Task {
            branch: "feat/x".to_string(),
            branch_sanitized: "feat-x".to_string(),
            state: aggregate_state(&[repo("api", Some(false))]),
            repos: vec![repo("api", Some(false))],
        };
        let hints = agent_hints(&workspace, &current, Some(&task));
        assert!(
            hints.iter().any(|h| h.contains("stack is not running")),
            "expected stack-down hint, got: {hints:?}"
        );
    }

    #[test]
    fn hints_include_the_remote_note_when_workspace_is_offloaded() {
        let workspace = Workspace {
            root: PathBuf::from("/x"),
            domain: None,
            runtime: "podman".to_string(),
            remote: Some(show::RemoteInfo {
                connection: "srv".to_string(),
                worktree_base_absolute: "/x".to_string(),
            }),
        };
        let current = Current {
            cwd: PathBuf::from("/x"),
            task: None,
            repo: None,
            task_dir: None,
            repo_worktree: None,
        };
        let hints = agent_hints(&workspace, &current, None);
        assert!(
            hints.iter().any(|h| h.contains("workspace.remote is set")),
            "expected remote hint, got: {hints:?}"
        );
    }
}
