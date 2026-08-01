pub mod init;
pub mod claude;
pub mod config;
pub mod remote_set;
pub mod new;
pub mod hooksweep;
pub mod teardown;
pub mod up;
pub mod down;
pub mod rm;
pub mod list;
pub mod clean;
pub mod exec;
pub mod setup;
pub mod remote_check;

#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub msg: String,
    /// When true, `main` exits with `code` but prints no failure banner — the
    /// command being wrapped already reported the failure (see `passthrough_exit`).
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

/// The machine-wide default remote host when no explicit host was given:
/// `$WS_REMOTE_HOST` (the env override), then the global config's `remote.host`
/// (`ws config set remote.host <h>`). Callers try their explicit flag/arg
/// first, completing the documented precedence: flag > env > global config.
/// A malformed global config is noted (not fatal) — the caller's own
/// "needs a host" error would otherwise mask the real problem.
pub(crate) fn default_remote_host() -> Option<String> {
    if let Some(h) = std::env::var("WS_REMOTE_HOST").ok().filter(|s| !s.is_empty()) {
        return Some(h);
    }
    match crate::config::GlobalConfig::load() {
        // A hand-edited empty host is treated as unset, same as the env var —
        // the caller's "needs a host" guidance beats an "unsafe host" refusal.
        Ok(g) => g.remote_host().filter(|h| !h.is_empty()).map(String::from),
        Err(e) => {
            crate::ui::note(&format!("global config: {e}"));
            None
        }
    }
}

/// Client-side readiness for a remote workspace (mutagen installed locally,
/// host reachable over SSH), checked when the remote block is configured
/// (`ws init --remote`, `ws remote set`) so a teammate hears about a bad setup
/// there instead of at their first `ws new`. Strict — `no_verify` (the callers'
/// `--no-verify` flag) skips it for the bootstrap case where the server isn't
/// provisioned yet. Owns the step UI too, so the ceremony can't drift between
/// callers. This is the *client* half; full server validation is `ws remote
/// check`.
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
    // Reuse the same read-only probes `ws remote check` runs, so there's one
    // source of truth for "is the remote reachable / can we sync".
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

/// Persist a workspace config to `<root>/.ws.json`, step UI included, so the
/// ops that write one (`ws init`, `ws remote set`/`unset`) can't drift on the
/// filename or the step label.
pub(crate) fn write_ws_json(
    root: &std::path::Path,
    cfg: &crate::config::WsConfig,
) -> anyhow::Result<()> {
    let step = crate::ui::step("cfg", ".ws.json");
    crate::config::write_json_pretty(&root.join(".ws.json"), cfg)?;
    step.ok();
    Ok(())
}

/// Exit with `code`, printing no failure banner — everything worth saying has
/// already been said (`silent: true`; `main` skips the banner). Used where `ws`
/// mirrors an outcome it merely relays: `ws exec` passing through a child's
/// exit code (the child reported its own failure), `ws config get` on an unset
/// key (git-parity exit 1, the stderr note said so).
pub(crate) fn passthrough_exit(code: i32) -> anyhow::Error {
    ExitError { code, msg: String::new(), silent: true }.into()
}

/// Return the interrupt error (exit 130) if a Ctrl-C has been observed. Called
/// by looping ops at each repo boundary so an interrupt stops the sweep cleanly
/// instead of proceeding into the next repo. Convention: 130 = SIGINT.
pub(crate) fn bail_if_interrupted() -> anyhow::Result<()> {
    if crate::signal::interrupted() {
        return Err(ExitError { code: 130, msg: "interrupted".into(), silent: false }.into());
    }
    Ok(())
}
