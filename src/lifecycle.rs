//! Run a repo's lifecycle event for a worktree, at whichever site its stack
//! lives. See ARCHITECTURE.md for how this sits between the ops and `hooks`.
//!
//! Two events deliberately stay outside the dispatch: `prepare` runs on the
//! CLIENT whatever the site (`ops::hooksweep::run_prepare` builds a `Site::Local`
//! `Ctx` for it), and a `kind: host` repo's lifecycle is a detached ws-owned
//! process rather than a blocking command, so `supervise` runs it.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

use crate::config::{RemoteConfig, WsConfig};
use crate::remote::Site;

/// The paths are the CLIENT's throughout — the server-side ones are derived from
/// them by `Site`, which owns that translation.
pub struct Ctx<'a> {
    pub cfg: &'a WsConfig,
    pub site: &'a Site,
    pub root: &'a Path,
    pub worktree: &'a Path,
    /// **Unsanitized** — the mutagen session name is keyed on it.
    pub branch: &'a str,
    /// The task dir / server worktree name.
    pub sanitized: &'a str,
    pub repo: &'a str,
}

impl Ctx<'_> {
    /// One resolution ladder for both sites; `None` means the caller renders its
    /// own dim skip.
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

    /// `extra_env` is only the caller's injection context — the base `WS_*`
    /// contract is added by the dispatch, with the paths of the site it runs at.
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

    /// An exit code is reported exactly as the local arm reports it; only a
    /// preamble or transport failure names the connection, since that is then the
    /// only place to look.
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

    /// The remote arm skips the [`run`](Self::run) preamble deliberately: this
    /// probe mutates nothing and runs once per repo behind `ws list`, so a flush +
    /// `.ws-hooks/` push + login-`PATH` harvest apiece would spend several SSH
    /// round trips on a status column. The timeout is enforced server-side there.
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

    /// The preamble every remote dispatch shares; returns the workspace root as
    /// the server sees it (its `WS_WORKSPACE_ROOT`).
    ///
    /// Public because the detached host-process launch needs the same staging
    /// without the dispatch that follows it (`ops::hooksweep::sweep_host`). It
    /// renders no failure of its own — a hook dispatch fails the whole op, a
    /// host-process launch fails one repo's step, so the caller decides.
    pub fn push_to_server(&self) -> Result<String> {
        // Must be absolute: a relative base leaves `WS_WORKSPACE_ROOT` relative, so a
        // hook resolving `$WS_WORKSPACE_ROOT/…` from the worktree cwd lands elsewhere.
        let base = self.site.root_at(self.cfg.remote.as_ref(), self.root);
        if !self.site.is_remote() {
            return Ok(base);
        }
        self.flush_to_server();
        // The workspace-root `.ws-hooks/` lives outside the repos, so it never rides
        // the worktree sync. Fatal: a hook that can't find its script fails anyway,
        // with a muddier error.
        crate::remote::push_ws_hooks(self.site, self.root, &base)?;
        Ok(base)
    }

    /// No-op for a local site — `mutagen` isn't even invoked, so a local workspace
    /// needs none installed.
    ///
    /// Best-effort: the session may not exist yet, or mutagen may be absent — a
    /// failed flush shouldn't block what follows.
    pub fn flush_to_server(&self) {
        if self.site.is_remote() {
            let _ = crate::mutagen::flush(&crate::mutagen::session_name(self.branch, self.repo));
        }
    }

    /// Infallible by construction: `Site::resolve` only yields `Remote` when
    /// `cfg.remote` is set.
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

    /// The remote path used to resolve through a ladder of its own, so an
    /// offloaded container repo declaring no `status` could get the compose
    /// default from one op and nothing from another.
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

    /// The migration shim reaches the remote path too, so a workspace that
    /// offloads *later* can't silently carry a dead hook file.
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
