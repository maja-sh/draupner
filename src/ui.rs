//! Visual language for user-facing output. See `spec-output-formatting.md`.
//!
//! Op code never calls `println!`/`eprintln!` directly. Every op uses these
//! helpers so tty detection, color palette, spinners, and non-tty fallback
//! all live in one place.

use console::{style, Term};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn tty() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| Term::stderr().features().colors_supported())
}

/// Op header — `◐ ws-new api,app test/foo` on tty, `ws-new: …` on non-tty.
pub fn header(op: &str, args: &str) {
    if tty() {
        eprintln!("{} {} {}", style("◐").cyan(), style(op).bold(), args);
    } else {
        eprintln!("{op}: {args}");
    }
}

/// Begin a step. Must be finished with `.ok(...)` / `.ok_with(...)` / `.fail(...)`.
#[must_use]
pub fn step(tag: &str, label: &str) -> Step {
    let start = Instant::now();
    let spinner = if tty() {
        let pb = ProgressBar::new_spinner();
        pb.enable_steady_tick(Duration::from_millis(80));
        pb.set_style(
            ProgressStyle::with_template("  {prefix} {spinner:.yellow} {msg}")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏↳"),
        );
        pb.set_prefix(format!("{}", style(pad_tag(tag)).green().bold()));
        pb.set_message(label.to_string());
        Some(pb)
    } else {
        eprintln!("  {}  ..  {}", pad_tag(tag), label);
        None
    };
    Step { tag: tag.to_string(), label: label.to_string(), start, spinner }
}

pub struct Step {
    tag: String,
    label: String,
    start: Instant,
    spinner: Option<ProgressBar>,
}

impl Step {
    pub fn ok(self) {
        self.finish_ok(None);
    }

    #[allow(dead_code)]
    pub fn ok_with(self, suffix: &str) {
        self.finish_ok(Some(suffix.to_string()));
    }

    pub fn fail(self, reason: &str) {
        let Step { tag, label, spinner, .. } = self;
        if let Some(pb) = spinner {
            pb.finish_and_clear();
        }
        if tty() {
            eprintln!(
                "  {} {} {} — {}",
                style(pad_tag(&tag)).green().bold(),
                style("✗").red().bold(),
                label,
                style(reason).red(),
            );
        } else {
            eprintln!("  {}  FAIL  {} — {}", pad_tag(&tag), label, reason);
        }
    }

    fn finish_ok(self, suffix: Option<String>) {
        let Step { tag, label, start, spinner } = self;
        let elapsed = start.elapsed();
        if let Some(pb) = spinner {
            pb.finish_and_clear();
        }
        let timing = format_timing(elapsed);
        if tty() {
            let mut line = format!(
                "  {} {} {}",
                style(pad_tag(&tag)).green().bold(),
                style("✓").green(),
                label,
            );
            if let Some(s) = suffix {
                line.push(' ');
                line.push_str(&s);
            }
            line.push(' ');
            line.push_str(&style(format!("({timing})")).dim().to_string());
            eprintln!("{line}");
        } else {
            let mut line = format!("  {}  ok  {}", pad_tag(&tag), label);
            if let Some(s) = suffix {
                line.push(' ');
                line.push_str(&s);
            }
            line.push_str(&format!(" [{timing}]"));
            eprintln!("{line}");
        }
    }
}

/// Stream one line of hook output.
pub fn hook_line(tag: &str, line: &str, _stderr: bool) {
    if tty() {
        eprintln!(
            "       {} [{}] {}",
            style("│").dim(),
            style(tag).dim().bold(),
            style(line).dim(),
        );
    } else {
        eprintln!("    [{tag}] {line}");
    }
}

pub fn summary_ok(lines: &[String]) {
    if tty() {
        eprintln!("\n{} {}", style("✓").green(), style("Done.").bold());
    } else {
        eprintln!("\nok: Done.");
    }
    for l in lines {
        eprintln!("    {l}");
    }
}

pub fn summary_fail(reason: &str) {
    if tty() {
        eprintln!("{} {}", style("✗").red().bold(), style(reason).red());
    } else {
        eprintln!("FAIL: {reason}");
    }
}

#[allow(dead_code)]
pub fn nudge(text: &str) {
    if tty() {
        eprintln!("\n{}", style(format!("Next: {text}")).dim().italic());
    } else {
        eprintln!("\nnext: {text}");
    }
}

fn pad_tag(tag: &str) -> String {
    if tag.len() >= 4 {
        tag.to_string()
    } else {
        format!("{tag:<4}")
    }
}

fn format_timing(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}s", secs as u64)
    }
}
