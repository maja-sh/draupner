mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn no_worktrees_is_nothing_to_clean() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    let out = f.run_ok(&["clean"]);
    assert!(stderr(&out).contains("nothing to clean"));
}

#[test]
fn preview_by_default_removes_nothing() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/done"]);
    let wt = ".worktrees/feat-done/api";
    f.git(wt, &["commit", "-q", "--allow-empty", "-m", "task work"]);
    f.git("api", &["merge", "-q", "feat/done"]);

    // No --force: a removable worktree is previewed, not removed.
    let out = f.run_ok(&["clean"]);
    let err = stderr(&out);

    assert!(f.path().join(wt).exists(), "preview must not remove anything");
    assert!(err.contains("would remove"), "expected a preview verdict in:\n{err}");
    assert!(err.contains("--force"), "expected a --force nudge in:\n{err}");
}

#[test]
fn clean_merged_worktree_is_removed_with_force() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/done"]);

    // Commit on the task branch, then merge it into main so its work is fully
    // contained in the default branch — the canonical "safe to drop" case.
    let wt = ".worktrees/feat-done/api";
    f.git(wt, &["commit", "-q", "--allow-empty", "-m", "task work"]);
    f.git("api", &["merge", "-q", "feat/done"]);

    let out = f.run_ok(&["clean", "--force"]);
    let err = stderr(&out);

    assert!(err.contains("remove"), "expected a remove verdict in:\n{err}");
    assert!(!f.path().join(".worktrees/feat-done/api").exists(), "merged worktree should be gone");
    assert!(err.contains("Removed 1"), "expected removal count in:\n{err}");
}

#[test]
fn dirty_worktree_is_kept() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/dirty"]);

    // Merge it (so only dirtiness, not unmerged work, is the reason it stays).
    let wt = ".worktrees/feat-dirty/api";
    f.git(wt, &["commit", "-q", "--allow-empty", "-m", "work"]);
    f.git("api", &["merge", "-q", "feat/dirty"]);
    // Now make the tree dirty.
    std::fs::write(f.path().join(wt).join("scratch.txt"), "wip").unwrap();

    let out = f.run_ok(&["clean"]);
    let err = stderr(&out);

    assert!(err.contains("keep"), "expected a keep verdict in:\n{err}");
    assert!(err.contains("dirty"), "expected 'dirty' reason in:\n{err}");
    assert!(f.path().join(wt).exists(), "dirty worktree must survive clean");
}

#[test]
fn unmerged_clean_worktree_is_kept() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/wip"]);

    // A committed-but-unmerged branch: clean tree, but work not in main.
    let wt = ".worktrees/feat-wip/api";
    f.git(wt, &["commit", "-q", "--allow-empty", "-m", "unmerged work"]);

    let out = f.run_ok(&["clean"]);
    let err = stderr(&out);

    assert!(err.contains("keep"), "expected a keep verdict in:\n{err}");
    assert!(err.contains("unmerged commits"), "expected 'unmerged commits' reason in:\n{err}");
    assert!(f.path().join(wt).exists(), "unmerged worktree must survive clean");
}

#[test]
fn mixed_removes_safe_keeps_unsafe() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    // Safe task: merged.
    f.run_ok(&["new", "api", "feat/safe"]);
    f.git(".worktrees/feat-safe/api", &["commit", "-q", "--allow-empty", "-m", "safe work"]);
    f.git("api", &["merge", "-q", "feat/safe"]);
    // Unsafe task: unmerged.
    f.run_ok(&["new", "api", "feat/keepme"]);
    f.git(".worktrees/feat-keepme/api", &["commit", "-q", "--allow-empty", "-m", "wip"]);

    let out = f.run_ok(&["clean", "--force"]);
    let err = stderr(&out);

    assert!(!f.path().join(".worktrees/feat-safe/api").exists(), "merged task should be removed");
    assert!(f.path().join(".worktrees/feat-keepme/api").exists(), "unmerged task should be kept");
    assert!(err.contains("Removed 1, kept 1"), "expected mixed summary in:\n{err}");
}

#[test]
fn never_committed_worktree_is_removed_with_force() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    // Fresh worktree, no commits beyond main, clean → safe to drop.
    f.run_ok(&["new", "api", "feat/empty"]);

    let out = f.run_ok(&["clean", "--force"]);
    let err = stderr(&out);

    assert!(!f.path().join(".worktrees/feat-empty/api").exists(), "never-committed worktree should be removed");
    assert!(err.contains("Removed 1"), "expected removal in:\n{err}");
}
