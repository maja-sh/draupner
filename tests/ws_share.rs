mod common;

use common::{stderr, WorkspaceFixture};

const ROUTED: &str = r#"{"domain":"myapp.test","repos":[{"name":"api",
    "services":{"web":{"http":true,"port":80}},
    "scripts":{"up":"echo up-ran"}}]}"#;

fn routed_fixture() -> WorkspaceFixture {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(ROUTED);
    f
}

#[test]
fn share_without_a_domain_is_a_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/x"]);
    let out = f.run(&["share", "feat/x"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("domain"), "{}", stderr(&out));
}

#[test]
fn share_refuses_an_offloaded_workspace() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"domain":"myapp.test","remote":{"connection":"srv"},"repos":["api"]}"#);
    let out = f.run(&["share", "feat/x"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("publish_address"), "{}", stderr(&out));
}

#[test]
fn share_refuses_a_wildcard_address() {
    let f = routed_fixture();
    f.run_ok(&["new", "api", "feat/x"]);
    let out = f.run(&["share", "feat/x", "--address", "0.0.0.0"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn share_without_worktrees_is_a_preflight_error() {
    let f = routed_fixture();
    let out = f.run(&["share", "feat/none"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("wsx new"), "{}", stderr(&out));
}

#[test]
fn a_shared_task_is_shared_again_on_up_and_unshares_cleanly() {
    let f = routed_fixture();
    f.run_ok(&["new", "api", "feat/x"]);
    f.run_ok(&["up", "feat/x"]);

    let out = f.run_ok(&["share", "feat/x"]);
    let err = stderr(&out);
    assert!(err.contains("https://feat-x.web.myapp.test"), "share must print the URL:\n{err}");
    assert!(err.contains("*.web.myapp.test"), "without a cert, share names the one it needs:\n{err}");
    assert!(f.path().join(".wsx-meta/shared/feat-x").is_file());

    let up = f.run_ok(&["up", "feat/x"]);
    assert!(stderr(&up).contains("shared"), "up must re-share a shared task:\n{}", stderr(&up));

    f.run_ok(&["unshare", "feat/x"]);
    assert!(!f.path().join(".wsx-meta/shared/feat-x").exists());
    let up = f.run_ok(&["up", "feat/x"]);
    assert!(!stderr(&up).contains("shared"), "an unshared task stays private:\n{}", stderr(&up));
}

#[test]
fn unshare_of_an_unshared_task_is_a_preflight_error() {
    let f = routed_fixture();
    f.run_ok(&["new", "api", "feat/x"]);
    let out = f.run(&["unshare", "feat/x"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn rm_forgets_the_share() {
    let f = routed_fixture();
    f.run_ok(&["new", "api", "feat/x"]);
    f.run_ok(&["share", "feat/x"]);
    f.run_ok(&["rm", "feat/x", "--force"]);
    assert!(!f.path().join(".wsx-meta/shared/feat-x").exists());
}
