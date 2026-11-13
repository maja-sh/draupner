//! A repo's own committed `.drup.json`: read from the main checkout, with the workspace entry laid over it.

mod common;

use common::{stderr, stdout, write_exec, WorkspaceFixture};

#[test]
fn a_bare_workspace_entry_defers_to_the_repos_own_config() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.write_repo_json("api", r#"{"scripts":{"up":"echo FROM-REPO-CONFIG"}}"#);
    f.run_ok(&["new", "api", "feat/own"]);

    let err = stderr(&f.run_ok(&["up", "feat/own"]));
    assert!(err.contains("FROM-REPO-CONFIG"), "the repo's own script should run:\n{err}");
}

#[test]
fn a_workspace_entry_overrides_per_key_and_keeps_the_rest() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_repo_json(
        "api",
        r#"{"env":{"KEPT":"repo","SWAPPED":"repo"},"scripts":{"up":"echo kept=$KEPT swapped=$SWAPPED"}}"#,
    );
    f.write_ws_json(r#"{"repos":[{"name":"api","env":{"SWAPPED":"workspace"}}]}"#);
    f.run_ok(&["new", "api", "feat/merge"]);

    let err = stderr(&f.run_ok(&["up", "feat/merge"]));
    assert!(
        err.contains("kept=repo swapped=workspace"),
        "one overridden key must not drop its siblings or the repo's script:\n{err}"
    );
}

// Every worktree of a repo that commits `.drup.json` carries a copy. Taking the nearest one for the workspace root would break each command run from inside a worktree — which is where they're run.
#[test]
fn a_worktrees_copy_of_the_repo_config_is_not_mistaken_for_the_workspace_root() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.write_repo_json("api", r#"{"scripts":{"up":"echo up"}}"#);
    f.git("api", &["add", ".drup.json"]);
    f.git("api", &["commit", "-q", "-m", "add drup config"]);
    f.run_ok(&["new", "api", "feat/inside"]);
    assert!(f.path().join(".worktrees/feat-inside/api/.drup.json").is_file());

    let out = f.run_in(".worktrees/feat-inside/api", &["path", "feat/inside", "-r", "api"]);
    assert!(out.status.success(), "drup should still find the real root: {}", stderr(&out));
    assert!(stdout(&out).trim().ends_with(".worktrees/feat-inside/api"), "{}", stdout(&out));
}

// "One config per repo per workspace": a branch that edits its `.drup.json` doesn't change what `drup` runs for that task.
#[test]
fn the_main_checkouts_config_wins_over_a_worktrees_edit() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.write_repo_json("api", r#"{"scripts":{"up":"echo FROM-MAIN-CHECKOUT"}}"#);
    f.run_ok(&["new", "api", "feat/drift"]);
    std::fs::write(
        f.path().join(".worktrees/feat-drift/api/.drup.json"),
        r#"{"scripts":{"up":"echo FROM-WORKTREE"}}"#,
    )
    .unwrap();

    let err = stderr(&f.run_ok(&["up", "feat/drift"]));
    assert!(err.contains("FROM-MAIN-CHECKOUT") && !err.contains("FROM-WORKTREE"), "{err}");
}

#[test]
fn a_hook_file_committed_beside_the_config_runs_from_the_worktree() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    let hooks = f.path().join("api/.drup-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    write_exec(&hooks.join("up.sh"), "#!/bin/sh\necho HOOK-AND-CONFIG-IN-REPO\n");
    f.write_repo_json("api", r#"{"scripts":{"up":".drup-hooks/up.sh"}}"#);
    f.git("api", &["add", "."]);
    f.git("api", &["commit", "-q", "-m", "add drup config and hook"]);
    f.run_ok(&["new", "api", "feat/whole"]);

    let err = stderr(&f.run_ok(&["up", "feat/whole"]));
    assert!(err.contains("HOOK-AND-CONFIG-IN-REPO"), "{err}");
}

#[test]
fn a_workspace_setting_in_a_repo_config_is_refused_by_name() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.write_repo_json("api", r#"{"domain":"myapp.test","scripts":{"up":"true"}}"#);

    let out = f.run(&["list"]);
    assert!(!out.status.success(), "a repo file carrying workspace settings must be refused");
    let err = stderr(&out);
    assert!(err.contains("`domain`") && err.contains("api/.drup.json"), "{err}");
}

#[test]
fn context_reports_where_each_repos_config_comes_from() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.add_repo("app");
    f.add_repo("docs");
    f.write_repo_json("api", r#"{"env":{"A":"1"},"scripts":{"up":"true"}}"#);
    f.write_repo_json("app", r#"{"scripts":{"up":"true"}}"#);
    f.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","env":{"A":"2","B":"3"}},"app","docs"]}"#,
    );

    let ctx: serde_json::Value =
        serde_json::from_str(&stdout(&f.run_ok(&["context"]))).expect("context is JSON");
    let repos = ctx["workspace"]["repos"].as_array().expect("workspace.repos");
    let by_name = |n: &str| repos.iter().find(|r| r["name"] == n).unwrap_or_else(|| panic!("{n} listed"));

    let api = by_name("api");
    assert!(api["config_file"].as_str().unwrap().ends_with("api/.drup.json"), "{api}");
    assert_eq!(api["workspace_overrides"], serde_json::json!(["kind", "env.A", "env.B"]));

    assert!(by_name("app")["config_file"].is_string());
    assert_eq!(by_name("app")["workspace_overrides"], serde_json::json!([]));

    assert!(by_name("docs")["config_file"].is_null());

    let hints = ctx["hints"].to_string();
    assert!(hints.contains("workspace_overrides"), "an override in effect must be called out: {hints}");
}

#[test]
fn init_leaves_a_repo_with_its_own_config_as_a_bare_entry() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_repo_json("api", r#"{"scripts":{"up":"true"}}"#);
    // A compose file is what makes `init` scaffold a `services` block into the workspace entry.
    std::fs::write(
        f.path().join("api/compose.yaml"),
        "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n",
    )
    .unwrap();

    f.run_ok(&["init"]);
    let ws: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(f.path().join(".drup.json")).unwrap()).unwrap();
    assert_eq!(ws["repos"], serde_json::json!(["api"]), "anything scaffolded would override the repo's file");
}
