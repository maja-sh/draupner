mod cli;
mod compat;
mod config;
mod hooks;
mod obs;
mod ops;
mod skills;
mod ui;
mod workspace;

fn main() {
    compat::check();
    obs::init_logging();
    if let Err(e) = cli::run() {
        crate::ui::summary_fail(&format!("{e:#}"));
        std::process::exit(1);
    }
}
