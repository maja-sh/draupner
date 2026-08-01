mod caddy;
mod cli;
mod compat;
mod compose;
mod config;
mod git;
mod hooks;
mod inject;
mod lifecycle;
mod mutagen;
mod obs;
mod ops;
mod ports;
mod probe;
mod remote;
mod setup;
mod signal;
mod skills;
mod supervise;
mod ui;
mod workspace;

fn main() {
    compat::check();
    obs::init_logging();
    signal::install();
    if let Err(e) = cli::run() {
        let exit = e.downcast_ref::<ops::ExitError>();
        let code = exit.map_or(1, |e| e.code);
        // A silent passthrough (e.g. `ws exec` mirroring a child's exit code)
        // carries its own code but should print no banner over the command's
        // own output. Everything else reports normally.
        if !exit.is_some_and(|e| e.silent) {
            crate::ui::summary_fail(&format!("{e:#}"));
        }
        std::process::exit(code);
    }
}
