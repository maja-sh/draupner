mod cli;
mod compat;
mod config;
mod hooks;
mod obs;
mod ops;
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
        crate::ui::summary_fail(&format!("{e:#}"));
        std::process::exit(code);
    }
}
