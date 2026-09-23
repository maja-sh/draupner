pub mod init;
pub mod claude;
pub mod config;
pub mod remote_set;
pub mod new;
pub mod add;
pub mod hooksweep;
pub mod teardown;
pub mod up;
pub mod down;
pub mod rm;
pub mod list;
pub mod path;
pub mod show;
pub mod context;
pub mod clean;
pub mod exec;
pub mod setup;
pub mod remote_check;

#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub msg: String,
    /// When true, `main` exits with `code` but prints no failure banner (see `passthrough_exit`).
    pub silent: bool,
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ExitError {}

pub(crate) fn preflight(msg: impl Into<String>) -> anyhow::Error {
    ExitError { code: 2, msg: msg.into(), silent: false }.into()
}

pub(crate) fn op_failure(msg: impl Into<String>) -> anyhow::Error {
    ExitError { code: 1, msg: msg.into(), silent: false }.into()
}

/// Machine-wide default remote host completing the precedence flag > env > global config; a malformed global config is noted, not fatal, so the caller's "needs a host" error isn't masked.
pub(crate) fn default_remote_host() -> Option<String> {
    if let Some(h) = std::env::var("WS_REMOTE_HOST").ok().filter(|s| !s.is_empty()) {
        return Some(h);
    }
    match crate::config::GlobalConfig::load() {
        // Empty host is treated as unset so the caller's "needs a host" guidance wins over an "unsafe host" refusal.
        Ok(g) => g.remote_host().filter(|h| !h.is_empty()).map(String::from),
        Err(e) => {
            crate::ui::note(&format!("global config: {e}"));
            None
        }
    }
}

/// Client-side readiness (mutagen local, SSH reachable) run at remote-config time so failure surfaces before the first `ws new`; `no_verify` escapes it for bootstrap. Server half is `ws remote check`.
pub(crate) fn verify_remote_client(
    r: &crate::config::RemoteConfig,
    no_verify: bool,
) -> anyhow::Result<()> {
    if no_verify {
        return Ok(());
    }
    let step = crate::ui::step("cfg", "remote reachable");
    match verify_probes(r) {
        Ok(()) => {
            step.ok();
            Ok(())
        }
        Err(e) => {
            step.fail("check failed");
            Err(e)
        }
    }
}

fn verify_probes(r: &crate::config::RemoteConfig) -> anyhow::Result<()> {
    // Reuse the probes `ws remote check` runs so there's one source of truth for reachability.
    if !crate::probe::mutagen_local().ok {
        return Err(preflight(
            "mutagen isn't installed — a remote workspace needs it locally to sync. \
             Install mutagen, or pass `--no-verify` to configure anyway.",
        ));
    }
    if !crate::probe::ssh_reachable(&r.connection).ok {
        return Err(preflight(format!(
            "can't reach `{}` over SSH — fix the connection (host/key/Tailscale), \
             or pass `--no-verify` to configure anyway.",
            r.connection
        )));
    }
    Ok(())
}

/// Shared flush-then-dispatch spine for remote lifecycle events; callers own their own skip decision, step UI, and bookkeeping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_remote_event(
    site: &crate::remote::Site,
    r: &crate::config::RemoteConfig,
    root: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
    cmd: &str,
    extra_env: &[(String, String)],
) -> anyhow::Result<()> {
    // Best-effort: session may not exist yet or mutagen may be absent, and a failed flush shouldn't block dispatch.
    let _ = crate::mutagen::flush(&crate::mutagen::session_name(branch, repo));
    // Absolute base — a relative one leaves `WS_WORKSPACE_ROOT` relative, so hooks resolving `$WS_WORKSPACE_ROOT/…` land in the wrong place.
    let base = site.abs_base(&r.worktree_base(root));
    // Ship workspace-root `.ws-hooks/` because those scripts live outside the repos and never ride the worktree sync; fatal here so the failure isn't muddied later.
    if let Err(e) = crate::remote::push_ws_hooks(site, root, &base) {
        return Err(op_failure(format!("{event} on {}: {e}", r.connection)));
    }
    match crate::remote::run_event(&r.connection, &base, sanitized, repo, event, cmd, extra_env) {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(op_failure(format!("{event} script exited {}", s.code().unwrap_or(-1)))),
        Err(e) => Err(op_failure(format!("{event} on {}: ssh error: {e}", r.connection))),
    }
}

/// Exit with `code` and no failure banner, for cases where `ws` merely relays an outcome already reported.
pub(crate) fn passthrough_exit(code: i32) -> anyhow::Error {
    ExitError { code, msg: String::new(), silent: true }.into()
}

/// Return the SIGINT exit (130) if Ctrl-C has been observed; called at repo boundaries so a sweep stops cleanly instead of rolling into the next repo.
pub(crate) fn bail_if_interrupted() -> anyhow::Result<()> {
    if crate::signal::interrupted() {
        return Err(ExitError { code: 130, msg: "interrupted".into(), silent: false }.into());
    }
    Ok(())
}

/// Best-effort `ws-net` creation when routing is on — a failure notes rather
/// than fails so a broken infra doesn't block worktree creation for a
/// stack that doesn't need routing.
pub(crate) fn ensure_ws_net_if_active(
    cfg: &crate::config::WsConfig,
    site: &crate::remote::Site,
) {
    if cfg.routing_active() {
        if let Err(e) = crate::caddy::ensure_net(site, cfg.runtime()) {
            crate::ui::note(&format!("ws-net: {e}"));
        }
    }
}
