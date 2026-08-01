//! One owner for "run this repo's lifecycle event for this worktree" — the
//! question every op asks, and until now answered twice.
//!
//! [`Ctx`] names the worktree and the [`Site`] its stack runs at. [`Ctx::command`]
//! resolves *what* the event runs — one ladder, the same at both sites — and
//! [`Ctx::run`] / [`Ctx::capture`] dispatch it: locally through `hooks` (`sh -c`
//! in the worktree), or on the server through `remote` (flush the sync, push
//! `.ws-hooks/`, harvest the login `PATH`, ssh).
//!
//! The remote-only steps live *inside* the `Remote` arm on purpose. That is what
//! the abstraction buys: a caller says what to run and where the stack is, never
//! how to reach it — and a `WS_*` key or a preamble step added here reaches every
//! op at once, instead of the four hand-written forks this replaces.
//!
//! Two things deliberately stay outside it. `prepare` runs on the CLIENT whatever
//! the site (see `ops::hooksweep::run_prepare`), so it builds a `Ctx` with
//! `Site::Local`. And a `kind: host` repo's lifecycle is a detached ws-owned
//! process, not a blocking command — `supervise` runs it, at the site the same
//! `Site` names.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

use crate::config::{RemoteConfig, WsConfig};
use crate::remote::Site;

/// The worktree a lifecycle event runs for, and where its stack lives.
///
/// The paths are the CLIENT's throughout — the server-side ones are derived from
/// them by `Site`, which owns that translation. Building one is cheap; ops build
/// it per repo in their sweep.
pub struct Ctx<'a> {
    pub cfg: &'a WsConfig,
    /// Where the stack runs, and thus where a resolved command is dispatched.
    pub site: &'a Site,
    /// The workspace root on this machine (the dir holding `.ws.json`).
    pub root: &'a Path,
    /// The repo's worktree on this machine.
    pub worktree: &'a Path,
    /// The task branch, **unsanitized** — the mutagen session name is keyed on it.
    pub branch: &'a str,
    /// The same branch, sanitized: the task dir / server worktree name.
    pub sanitized: &'a str,
    pub repo: &'a str,
}

impl Ctx<'_> {
    /// The shell command `event` runs for this repo, or `None` when it declares
    /// none (the caller renders its own dim skip). One resolution ladder for both
    /// sites: the declared `scripts` entry, else a container repo's compose
    /// default (see [`crate::hooks::resolve`]).
    ///
    /// Container-ness is read from the *local* worktree — the synced server copy
    /// is the same tree, and it's the only one that exists before the first sync.
    pub fn command(&self, event: &str) -> Result<Option<String>> {
        crate::hooks::resolve(
            self.cfg,
            self.root,
            self.repo,
            event,
            self.cfg.is_container_repo(self.repo, self.worktree),
        )
    }

    /// Run `cmd` as this repo's `event` where the stack lives, streaming its
    /// output through `ui::hook_line` either way. `extra_env` carries the
    /// injection context (`WS_HOST_*`/`WS_PORT_*` + declared `env`) the caller
    /// computed for this repo; the base `WS_*` contract is added by the dispatch
    /// itself, with the paths of the site it runs at.
    pub fn run(&self, event: &str, cmd: &str, extra_env: &[(String, String)]) -> Result<()> {
        match self.site {
            Site::Local => crate::hooks::run_command(
                self.root,
                self.worktree,
                self.repo,
                self.sanitized,
                event,
                cmd,
                extra_env,
            ),
            Site::Remote { .. } => self.run_at_server(event, cmd, extra_env),
        }
    }

    /// The remote arm of [`run`](Self::run): the shared preamble
    /// ([`push_to_server`](Self::push_to_server) — flush, resolve the server root,
    /// push `.ws-hooks/`), then the command over SSH in the synced worktree, with
    /// the outcome mapped to a `Result`.
    ///
    /// The failure text is the caller-facing one: an exit code reads the same as
    /// the local arm's (`"<event> script exited N"`, so a step line can't tell you
    /// where it ran when that doesn't matter), while a preamble or transport
    /// failure names the connection, which is the only place to look.
    fn run_at_server(&self, event: &str, cmd: &str, extra_env: &[(String, String)]) -> Result<()> {
        let r = self.remote_config();
        let base = self
            .push_to_server()
            .map_err(|e| crate::ops::op_failure(format!("{event} on {}: {e}", r.connection)))?;
        match crate::remote::run_event(
            &r.connection,
            &base,
            self.sanitized,
            self.repo,
            event,
            cmd,
            extra_env,
        ) {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(crate::ops::op_failure(format!(
                "{event} script exited {}",
                s.code().unwrap_or(-1)
            ))),
            Err(e) => Err(crate::ops::op_failure(format!(
                "{event} on {}: ssh error: {e}",
                r.connection
            ))),
        }
    }

    /// Run a resolved read-only `status`-style command and capture its first
    /// non-empty stdout line, giving up after `timeout`. Used by `ws list`.
    ///
    /// The remote arm skips the [`run`](Self::run) preamble deliberately: this
    /// probe mutates nothing and runs once per repo behind `ws list`, so a flush +
    /// `.ws-hooks/` push + login-`PATH` harvest apiece would spend several SSH
    /// round trips on a status column. The bound is enforced server-side instead,
    /// so a wedged probe can't stall the listing either way.
    pub fn capture(
        &self,
        event: &str,
        cmd: &str,
        extra_env: &[(String, String)],
        timeout: Duration,
    ) -> Option<String> {
        match self.site {
            Site::Local => crate::hooks::capture_command(
                self.root,
                self.worktree,
                self.repo,
                self.sanitized,
                event,
                cmd,
                timeout,
                extra_env,
            ),
            Site::Remote { .. } => {
                let r = self.remote_config();
                crate::remote::capture_status(
                    &r.connection,
                    &self.site.root_at(Some(r), self.root),
                    self.sanitized,
                    self.repo,
                    extra_env,
                    cmd,
                    timeout.as_secs(),
                )
            }
        }
    }

    /// The preamble every remote dispatch shares: flush the worktree's mutagen
    /// sync so the server sees the latest files, resolve the workspace root *as
    /// the server sees it*, and push the workspace-root `.ws-hooks/` there.
    /// Returns that root (the server's `WS_WORKSPACE_ROOT`).
    ///
    /// Public because the detached host-process launch needs the same staging
    /// without the dispatch that follows it (`ops::hooksweep::sweep_host`) — one
    /// preamble, not two spellings of it. It renders no failure of its own: the
    /// hook dispatch fails the whole op while the host-process launch fails one
    /// repo's step, so the caller decides.
    ///
    /// No-op for a local site (nothing to flush to, nothing to push), so a caller
    /// that has no site branch of its own needs none.
    pub fn push_to_server(&self) -> Result<String> {
        // Absolute, home-resolved base — as `ws list`/`status` already use. A relative
        // base makes `cd` work only by luck (SSH lands in `$HOME`) and, worse, leaves
        // `WS_WORKSPACE_ROOT` relative, so a hook resolving `$WS_WORKSPACE_ROOT/…` from
        // the worktree cwd lands in the wrong place.
        let base = self.site.root_at(self.cfg.remote.as_ref(), self.root);
        if !self.site.is_remote() {
            return Ok(base);
        }
        self.flush_to_server();
        // Ship the workspace-root `.ws-hooks/` to the server so a hook here can call
        // `$WS_WORKSPACE_ROOT/.ws-hooks/…` — scripts that live outside the repos (and
        // so never ride the worktree sync). Before dispatch, and fatal: a hook that
        // can't find its script would fail anyway, with a muddier error.
        crate::remote::push_ws_hooks(self.site, self.root, &base)?;
        Ok(base)
    }

    /// Push this worktree's local edits to the server, so what runs there next
    /// sees them. No-op for a local site — `mutagen` isn't even invoked, so a
    /// local workspace needs none installed.
    ///
    /// Part of [`push_to_server`](Self::push_to_server), and callable on its own by
    /// an op that has to make the server current *before* something other than a
    /// dispatch reads it (`ws up` derives a repo's services from the server-side
    /// worktree). Doing both then flushes twice, cheaply and deliberately.
    ///
    /// Best-effort: the session may not exist yet, or mutagen may be absent (see
    /// `ws exec`) — a failed flush shouldn't block what follows.
    pub fn flush_to_server(&self) {
        if self.site.is_remote() {
            let _ = crate::mutagen::flush(&crate::mutagen::session_name(self.branch, self.repo));
        }
    }

    /// The `remote` block behind a `Remote` site. Infallible by construction:
    /// `Site::resolve` only yields `Remote` when `cfg.remote` is set.
    fn remote_config(&self) -> &RemoteConfig {
        self.cfg.remote.as_ref().expect("remote site ⇒ a remote config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_site() -> Site {
        Site::Remote { connection: "srv".to_string(), home: "/home/test".to_string() }
    }

    fn ctx<'a>(cfg: &'a WsConfig, site: &'a Site, root: &'a Path, wt: &'a Path) -> Ctx<'a> {
        Ctx { cfg, site, root, worktree: wt, branch: "feat/x", sanitized: "feat-x", repo: "api" }
    }

    /// Resolution is the dispatcher's, not the site's. The two used to differ:
    /// the remote path resolved through a ladder of its own, so an offloaded
    /// container repo declaring no `status` got the compose default from one op
    /// (`ws list`) and, on the `post-create` path, no default at all — a
    /// divergence that only stayed invisible because `post-create` has no default
    /// to miss.
    #[test]
    fn a_container_default_resolves_the_same_at_both_sites() {
        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","kind":"container","scripts":{"up":"echo up"}}],
                "remote":{"connection":"srv","workspace_path":"ws"}}"#,
        )
        .expect("parse config");
        let root = Path::new("/ws");
        let wt = Path::new("/ws/.worktrees/feat-x/api");
        let local = Site::Local;
        let remote = remote_site();

        for event in ["up", "down", "status", "post-remove", "post-create"] {
            let at_local = ctx(&cfg, &local, root, wt).command(event).expect("resolve");
            let at_remote = ctx(&cfg, &remote, root, wt).command(event).expect("resolve");
            assert_eq!(at_local, at_remote, "{event} must resolve the same at either site");
        }
        // The defaults an offloaded container repo now gets, spelled out.
        assert_eq!(
            ctx(&cfg, &remote, root, wt).command("status").expect("resolve").as_deref(),
            Some("podman compose ps -q | wc -l")
        );
        assert_eq!(
            ctx(&cfg, &remote, root, wt).command("post-create").expect("resolve"),
            None,
            "post-create has no default to inherit — the one event where the old \
             remote-only ladder happened to agree"
        );
    }

    /// The migration shim reaches the remote path too now. A leftover executable
    /// hook file never ran on an offloaded workspace, so this changes nothing that
    /// worked — it just stops a workspace that offloads *later* from silently
    /// carrying a dead hook.
    #[test]
    fn a_leftover_hook_file_is_refused_at_either_site() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = dir.path().join(".ws-hooks").join("api");
        std::fs::create_dir_all(&hooks).expect("mkdir");
        let file = hooks.join("up");
        std::fs::write(&file, "#!/bin/sh\necho hi\n").expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api"}],"remote":{"connection":"srv","workspace_path":"ws"}}"#,
        )
        .expect("parse config");
        let wt = dir.path().join(".worktrees/feat-x/api");
        let remote = remote_site();
        assert!(ctx(&cfg, &remote, dir.path(), &wt).command("up").is_err());
    }
}
