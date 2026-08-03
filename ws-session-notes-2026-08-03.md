# ws — session notes, 2026-08-03

Written to be self-contained so this can be picked up cold on the server.
All of it applies to `github.com/hbackman/workspaces`.

Base for everything below: `04aacc3` (`Release v0.5.2`, = `origin/main` as of today,
= the binary currently installed at `~/.local/bin/ws`).

---

## 1. Identity / history — read this before touching git

**The rewrite already happened upstream. Do not force-push a rewritten local branch.**

Validated today:

```
commits on origin/main NOT in local main:  140
commits in local main NOT on origin/main:   88
identities present on origin/main:          Maja Backman <me@maja.sh>   (only one)
```

Every commit on `origin/main` is already `Maja Backman <me@maja.sh>`. There is no
deadname upstream.

The only place the old identity survived was the **local** checkout at
`~/workspace2/workspaces`, which was:

- frozen at 2026-07-30
- authored on the server (`Hampus Backman <hbackman@backman-server.hbackman.com>`)
- 140 commits behind, incl. 18 releases through v0.5.2, the docs commit, and the
  DNS resolver-defence work

Rewriting that branch and force-pushing would have replaced a correct, current
history with a re-authored copy of a stale one and destroyed those 140 commits.

**Of the 88 "unpushed" local commits, 87 are already upstream** under rebased
hashes (identical content, identical author dates, new identity). The 88th is
`3f34378 Import specs + vault-refs from backman-server` — 9 lines: four
`.gitignore` entries (all present upstream) and a three-line README stub
superseded by the real README. Its `specs/INDEX.md` link points at a directory
that does not exist upstream. Nothing to preserve.

Correct action on whichever machine holds the clone:

```bash
git branch backup/pre-reset-main main     # cheap, keeps the old line recoverable
git reset --hard origin/main
git config user.name  "Maja Backman"
git config user.email "me@maja.sh"
```

No force push anywhere.

### Still outstanding

Global git config on the work laptop is `Hampus Backman <hampus@myapp.com>`,
which is why new commits in the SafetyAmp repos carry that name. Name and email
are separable — `user.name = "Maja Backman"` globally fixes the name everywhere
without touching `@myapp.com` routing. Rewriting existing SafetyAmp history is
a much larger question (shared branches, open PRs, force-push blast radius) and
was deliberately not started.

---

## 2. Port suffix missing from routing output — FIXED, patch below

### The bug

`ws up` and `ws remote check` both print where to reach a worktree and both omit
the port. Caddy binds **8443** by default (this setup uses **8444**, per
`~/.config/ws/https_port`), so following either line verbatim gets a refused
connection.

```
ws up            api-docs ✓ routed chore-test.docs.myapp.test
ws remote check  local routing ready — reach a worktree at https://<branch>.<svc>.myapp.test
```

`setup.rs` already had the right answer — a private `port_suffix()` that emits
`:8444` and collapses to empty on `:443`. It just wasn't reachable from the other
two call sites.

### The fix

Move `port_suffix` to `caddy.rs` beside `https_port()` (which it derives from),
and call it from the two sites that skipped it.

Verified live against the real config:

```
before   …reach a worktree at https://<branch>.<svc>.myapp.test
after    …reach a worktree at https://<branch>.<svc>.myapp.test:8444
```

Test suite: **159 passed, 3 failed** — all three failures pre-existing (see §4),
confirmed by stashing the patch and re-running on pristine `origin/main`.

New regression test pins `WS_HTTPS_PORT=8443` so it does not inherit that
flakiness; confirmed it fails without the fix and passes with it.

### Patch (apply to 04aacc3)

```diff
diff --git a/src/caddy.rs b/src/caddy.rs
@@ -126,6 +126,19 @@ pub fn https_port() -> u16 {
         .unwrap_or(DEFAULT_HTTPS_PORT)
 }
 
+/// The `:port` suffix a browser URL carries, empty for the standard `:443`.
+///
+/// Lives beside [`https_port`] because every line that tells a reader where to
+/// reach a worktree has to spell the port the same way, and the default is the
+/// one part of the URL they cannot guess.
+pub fn port_suffix(port: u16) -> String {
+    if port == 443 {
+        String::new()
+    } else {
+        format!(":{port}")
+    }
+}
+
 /// The address Caddy publishes its HTTPS port on. Local is always loopback;

diff --git a/src/ops/hooksweep.rs b/src/ops/hooksweep.rs
@@ -378,7 +378,13 @@ fn sync_routes(
     let step = crate::ui::step(repo, "routed");
     match err {
         None => {
-            step.ok_with(&hosts.join(", "));
+            // Carry the port. This is the line that tells a reader where the
+            // worktree is, and the port is the one part of it they can't guess —
+            // Caddy binds 8443 by default, so a bare hostname reads as `:443` and
+            // refuses the connection.
+            let suffix = crate::caddy::port_suffix(crate::caddy::https_port());
+            let with_port: Vec<String> = hosts.iter().map(|h| format!("{h}{suffix}")).collect();
+            step.ok_with(&with_port.join(", "));
             true
         }

diff --git a/src/ops/remote_check.rs b/src/ops/remote_check.rs
@@ -132,7 +132,8 @@ fn check_local(cfg: &config::WsConfig, verbose: bool) -> Result<()> {
     crate::ui::summary_ok(&[format!(
-        "local routing ready — reach a worktree at https://<branch>.<svc>.{domain}"
+        "local routing ready — reach a worktree at https://<branch>.<svc>.{domain}{}",
+        crate::caddy::port_suffix(report.port)
     )]);

diff --git a/src/setup.rs b/src/setup.rs
@@ -362,7 +362,7 @@ pub fn run(
             "routing is ready — reach a worktree at https://<branch>.<svc>.{domain}{}",
-            port_suffix(report.port)
+            crate::caddy::port_suffix(report.port)
         )]);

@@ -790,14 +790,6 @@ fn linux_client_commands(report: &Report, domain: &str) -> Vec<String> {
-/// The `:port` suffix a browser URL carries, empty for the standard `:443`.
-fn port_suffix(port: u16) -> String {
-    if port == 443 {
-        String::new()
-    } else {
-        format!(":{port}")
-    }
-}

@@ -959,8 +951,8 @@ mod tests {
     fn port_suffix_is_empty_only_for_443() {
-        assert_eq!(port_suffix(443), "");
-        assert_eq!(port_suffix(8443), ":8443");
+        assert_eq!(crate::caddy::port_suffix(443), "");
+        assert_eq!(crate::caddy::port_suffix(8443), ":8443");
     }

diff --git a/tests/ws_up.rs b/tests/ws_up.rs
@@ -187,8 +187,14 @@ fn up_succeeds_and_routes_when_caddy_is_reachable() {
     f.run_ok(&["new", "api", "feat/route"]);
 
-    let err = stderr(&f.run_ok(&["up", "feat/route"]));
-    assert!(err.contains("feat-route.web.myapp.test"), "expected the routed hostname in:\n{err}");
+    // Pinned so the assertion doesn't depend on whether this machine opted into a
+    // non-default HTTPS port.
+    let err = stderr(&f.run_with_env(&["up", "feat/route"], &[("WS_HTTPS_PORT", "8443")]));
+    assert!(
+        err.contains("feat-route.web.myapp.test:8443"),
+        "the routed line must carry the port — it is the part of the URL a reader \
+         cannot guess, and Caddy does not bind 443 by default:\n{err}"
+    );
 }
```

---

## 3. `ws list` and `ws rm` disagree about stale worktrees — DIAGNOSED, not fixed

### Symptom

```
$ ws list
  chore/sa-2956-incident-api-docs
    api-docs clean (unmanaged, chore/sa-2956-incident-api-docs)

$ ws rm chore/sa-2956-incident-api-docs
✗ no worktrees for chore/sa-2956-incident-api-docs; nothing to remove
```

### Cause

A stale git worktree registration in `api-docs` pointing at
`/Users/hbackman/workspace/myapp/.worktrees/…` — a home directory that does
not exist on that machine. Git flags it `prunable`.

- **`ws list`** enumerates from git (`workspace::worktrees`), so it sees the
  registration. It renders `(unmanaged)` because the path is not under this
  root's `.worktrees/`, so no task name can be derived.
- **`ws rm`** calls `workspace::all_worktrees_for`, which is
  `list_child_dirs(&task_dir(root, branch))` — a **filesystem scan**. Nothing on
  disk, so nothing to remove.

The doc comment on `workspace::worktrees` already states the rule this violates:

> the single definition of "a worktree exists", **shared by every op that
> enumerates**. Sourced from git, not from `.worktrees/` on disk […] and **git
> alone knows a registration is stale**.

`rm` is the one op that doesn't use it, and a stale registration is exactly the
case that comment was written about.

Cleared for now with `git -C api-docs worktree prune`.

### Proposed fix — and the trap in it

**Do not simply reimplement `all_worktrees_for` on top of `worktrees()`.** The
other caller depends on the filesystem semantics *deliberately*.
`src/ops/remote_set.rs` (around line 201):

> Deliberately the **filesystem** walk, not `workspace::worktrees`: this is the
> asymmetry between creating and destroying. `set` must be strict (only sync what
> git calls a worktree), but `unset` must be broad — a directory git doesn't know
> may still have a live session pointing at the server […] Narrowing this to git
> would strand exactly those.

So the two lookups need **splitting**, not swapping:

1. Keep the filesystem walk for `remote_set::unset`. Rename it to say so —
   e.g. `all_worktree_dirs_for` — matching the existing convention on
   `all_task_dirs` ("A *filesystem* view — see [`worktrees`] for the
   authoritative one").
2. Add a git-sourced `worktrees_for(root, cfg, branch)` and point **only** `rm`
   at it. `rm` already has `cfg` in scope (loaded one line above the call).

Match on the branch git reports **or** the task dir name:

```rust
pub fn worktrees_for(
    root: &Path,
    cfg: &crate::config::WsConfig,
    branch: &str,
) -> Vec<(String, PathBuf)> {
    let task = sanitize_branch(branch);
    worktrees(root, cfg)
        .into_iter()
        .filter(|w| w.branch.as_deref() == Some(branch) || w.task.as_deref() == Some(task.as_str()))
        .map(|w| (w.repo, w.path))
        .collect()
}
```

The task-name fallback is load-bearing twice over: a **detached** worktree has no
branch to match on, and `remote_set` passes an already-sanitized task name
(sanitizing is idempotent, so it matches through the same path).

### Second half — `rm` still can't finish the job

With the above, `rm` *finds* the stale entry, then fails in
`teardown::remove_worktree`, because `git worktree remove` needs a working tree
that exists. It already prints "try `git worktree prune`". Better to do it:

```rust
// A registration whose directory is gone can't be removed — prune is the
// operation that clears it, and it only ever drops registrations whose paths
// are already absent.
if !wt.exists() {
    let pruned = Command::new("git")
        .arg("-C").arg(main)
        .args(["worktree", "prune"])
        .output()?;
    if pruned.status.success() {
        return Ok(());
    }
}
```

Suggested test (`tests/ws_rm.rs`): `ws new`, delete the worktree directory by
hand leaving the registration, then assert `ws rm` succeeds.

---

## 4. Pre-existing test failures — separate bug

```
caddy::tests::admin_port_seam_wins_and_zero_falls_through
caddy::tests::https_port_env_overrides_the_default
caddy::tests::https_port_ignores_a_zero_value
```

These read the **real** `~/.config/ws/https_port` rather than an isolated one, so
they assert `8443` and get `8444` on any machine that opted into a non-default
port via `ws setup --port`. Confirmed failing on pristine `origin/main` with no
patch applied. Not caused by anything in §2. Worth fixing by isolating the config
dir in the test harness.

---

## 5. Other verified facts worth keeping

- `Report.port` is set from `crate::caddy::https_port()` in `setup::probe`, so
  `report.port` and `https_port()` are always the same value — either is
  correct at a call site.
- `ws` auto-merges a family-matched `<primary>.override.{yaml,yml}` sibling
  ahead of its own generated override (`src/compose.rs`), so a committed
  `docker-compose.override.yaml` applies identically under plain compose and
  under `ws up`.
- `.ws-meta/<branch>-<repo>.json` **is** the port reservation — "the recorded
  allocation is reused; only a genuinely new service is allocated"
  (`src/ports.rs`). Container restarts, and even full `ws down`/`ws up`, keep the
  same host port. Only deleting `.ws-meta` moves it.
- `ws` has **no submodule handling at all** — the string does not appear in
  `src/`, `docs/`, `README.md` or `CLAUDE.md`. `git worktree add` leaves
  submodules uninitialised, so any repo with one needs a `prepare` hook.
