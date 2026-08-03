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
    /// Suppresses `main`'s failure banner, for outcomes the wrapped command has
    /// already reported itself (see `passthrough_exit`).
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

/// The env/global-config half of the documented `flag > env > global config`
/// precedence; callers try their explicit flag first. A malformed global config
/// is noted rather than fatal, so it doesn't hide behind the caller's own
/// "needs a host" error.
pub(crate) fn default_remote_host() -> Option<String> {
    if let Some(h) = std::env::var("WS_REMOTE_HOST").ok().filter(|s| !s.is_empty()) {
        return Some(h);
    }
    match crate::settings::GlobalConfig::load() {
        // An empty host counts as unset: the caller's "needs a host" guidance
        // beats an "unsafe host" refusal.
        Ok(g) => g.remote_host().filter(|h| !h.is_empty()).map(String::from),
        Err(e) => {
            crate::ui::note(&format!("global config: {e}"));
            None
        }
    }
}

/// Client-side readiness for a remote workspace, run when the remote block is
/// configured so a bad setup surfaces there rather than at the first `ws new`.
/// `no_verify` exists for the bootstrap case where the server isn't provisioned
/// yet. The *client* half only; full server validation is `ws remote check`.
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

pub(crate) fn write_ws_json(
    root: &std::path::Path,
    cfg: &crate::config::WsConfig,
) -> anyhow::Result<()> {
    let step = crate::ui::step("cfg", ".ws.json");
    crate::config::write_json_pretty(&root.join(".ws.json"), cfg)?;
    step.ok();
    Ok(())
}

/// Exit with `code` and no failure banner, for outcomes `ws` merely relays:
/// `ws exec` passing through a child's exit code, `ws config get` on an unset
/// key. Everything worth saying has already been said.
pub(crate) fn passthrough_exit(code: i32) -> anyhow::Error {
    ExitError { code, msg: String::new(), silent: true }.into()
}

/// Called by looping ops at each repo boundary, so a Ctrl-C stops the sweep
/// between repos rather than mid-repo. Exit 130 is the SIGINT convention.
pub(crate) fn bail_if_interrupted() -> anyhow::Result<()> {
    if crate::signal::interrupted() {
        return Err(ExitError { code: 130, msg: "interrupted".into(), silent: false }.into());
    }
    Ok(())
}
