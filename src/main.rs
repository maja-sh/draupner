mod cli;
mod compat;
mod config;
mod hooks;
mod mutagen;
mod obs;
mod ops;
mod remote;
mod signal;
mod skills;
mod ui;
mod workspace;

fn main() {
    compat::check();
    obs::init_logging();
    signal::install();
    if let Err(e) = cli::run() {
        let code = e.downcast_ref::<ops::ExitError>().map_or(1, |e| e.code);
        // An empty message is a deliberate silent passthrough (e.g. `ws exec`
        // mirroring a child's exit code) — don't print a spurious banner.
        let msg = format!("{e:#}");
        if !msg.is_empty() {
            crate::ui::summary_fail(&msg);
        }
        std::process::exit(code);
    }
}
