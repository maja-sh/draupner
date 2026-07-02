//! Visual language for user-facing output. See `spec-output-formatting.md`.
//!
//! Op code never calls `println!`/`eprintln!` directly. Every op uses these
//! helpers so tty detection, color palette, spinners, and non-tty fallback
//! all live in one place.

use console::{style, Term};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Left-pad width for the step-line tag column. Widest tag currently in use is
/// `"skills"` (6). Longer tags overflow naturally.
const TAG_WIDTH: usize = 6;

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
///
/// If a `Step` is dropped without a terminal call (e.g. an `?` short-circuit
/// upstream), `Drop` clears the spinner and emits an aborted line so the
/// caller doesn't leave a half-drawn spinner on the terminal.
#[must_use = "Step must be completed with .ok() / .ok_with() / .fail(); dropping without a call emits an aborted line."]
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
        None
    };
    Step {
        tag: tag.to_string(),
        label: label.to_string(),
        start,
        spinner,
        finished: false,
    }
}

pub struct Step {
    tag: String,
    label: String,
    start: Instant,
    spinner: Option<ProgressBar>,
    finished: bool,
}

impl Step {
    pub fn ok(mut self) {
        self.finished = true;
        self.finish_ok_inner(None);
    }

    #[allow(dead_code)]
    pub fn ok_with(mut self, suffix: &str) {
        self.finished = true;
        self.finish_ok_inner(Some(suffix.to_string()));
    }

    pub fn fail(mut self, reason: &str) {
        self.finished = true;
        if let Some(pb) = self.spinner.take() {
            pb.finish_and_clear();
        }
        if tty() {
            eprintln!(
                "  {} {} {} — {}",
                style(pad_tag(&self.tag)).green().bold(),
                style("✗").red().bold(),
                self.label,
                style(reason).red(),
            );
        } else {
            eprintln!("  {}  FAIL  {} — {}", pad_tag(&self.tag), self.label, reason);
        }
    }

    fn finish_ok_inner(&mut self, suffix: Option<String>) {
        let elapsed = self.start.elapsed();
        if let Some(pb) = self.spinner.take() {
            pb.finish_and_clear();
        }
        let timing = format_timing(elapsed);
        if tty() {
            let mut line = format!(
                "  {} {} {}",
                style(pad_tag(&self.tag)).green().bold(),
                style("✓").green(),
                self.label,
            );
            if let Some(s) = suffix {
                line.push(' ');
                line.push_str(&s);
            }
            line.push(' ');
            line.push_str(&style(format!("({timing})")).dim().to_string());
            eprintln!("{line}");
        } else {
            let mut line = format!("  {}  ok  {}", pad_tag(&self.tag), self.label);
            if let Some(s) = suffix {
                line.push(' ');
                line.push_str(&s);
            }
            line.push_str(&format!(" [{timing}]"));
            eprintln!("{line}");
        }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(pb) = self.spinner.take() {
            pb.finish_and_clear();
        }
        // Step dropped without a terminal call — usually a `?` upstream. Emit
        // an aborted line so the user isn't left with a silent gap between
        // the header and the propagated error.
        if tty() {
            eprintln!(
                "  {} {} {}",
                style(pad_tag(&self.tag)).green().bold(),
                style("✗").red().bold(),
                self.label,
            );
        } else {
            eprintln!("  {}  FAIL  {}", pad_tag(&self.tag), self.label);
        }
    }
}

/// Stream one line of hook output. The `stderr` flag is currently unused —
/// see spec-output-formatting §"Hook-streamed line" (no distinct styling in v1).
pub fn hook_line(tag: &str, line: &str, _stderr: bool) {
    let cleaned = strip_control(line);
    if tty() {
        eprintln!(
            "       {} [{}] {}",
            style("│").dim(),
            style(tag).dim().bold(),
            style(&cleaned).dim(),
        );
    } else {
        eprintln!("    [{tag}] {cleaned}");
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

pub fn nudge(text: &str) {
    if tty() {
        eprintln!("\n{}", style(format!("Next: {text}")).dim().italic());
    } else {
        eprintln!("\nnext: {text}");
    }
}

fn pad_tag(tag: &str) -> String {
    format!("{tag:<width$}", width = TAG_WIDTH)
}

fn format_timing(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}s", secs as u64)
    }
}

/// Strip ANSI CSI/OSC escape sequences and other C0/C1 control bytes from a
/// hook line. Prevents hooks from clearing the terminal, faking op status
/// lines, or otherwise scribbling on ws's own output.
fn strip_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // ESC — swallow a CSI/OSC sequence.
                match chars.next() {
                    Some('[') => {
                        // CSI: parameter/intermediate bytes then a final byte in 0x40..=0x7e.
                        for c in chars.by_ref() {
                            if matches!(c, '\x40'..='\x7e') {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC: terminated by BEL (\x07) or ST (ESC \).
                        let mut prev = '\0';
                        for c in chars.by_ref() {
                            if c == '\x07' || (prev == '\x1b' && c == '\\') {
                                break;
                            }
                            prev = c;
                        }
                    }
                    Some(_) => {
                        // Single-byte escape (e.g. ESC c reset). Drop the byte.
                    }
                    None => {}
                }
            }
            // Other C0 controls except tab / newline: drop.
            '\x00'..='\x08' | '\x0b'..='\x1f' | '\x7f' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_control_removes_csi() {
        assert_eq!(strip_control("\x1b[2Jclean"), "clean");
        assert_eq!(strip_control("red\x1b[31mgreen\x1b[0m"), "redgreen");
    }

    #[test]
    fn strip_control_removes_osc_title() {
        assert_eq!(strip_control("hi\x1b]0;pwned\x07there"), "hithere");
    }

    #[test]
    fn strip_control_keeps_normal_text_and_tabs() {
        assert_eq!(strip_control("hello\tworld\n"), "hello\tworld\n");
    }

    #[test]
    fn pad_tag_pads_short_and_leaves_long_alone() {
        assert_eq!(pad_tag("cfg"), "cfg   ");
        assert_eq!(pad_tag("skills"), "skills");
        assert_eq!(pad_tag("post-create"), "post-create");
    }
}
