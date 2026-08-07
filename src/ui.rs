//! Op code never calls `println!`/`eprintln!` directly — tty detection, palette, spinners, and non-tty fallback all live here.

use console::{style, Term};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Sized for the widest tag currently in use (`"skills"`); longer tags overflow naturally.
const TAG_WIDTH: usize = 6;

fn tty() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| Term::stderr().features().colors_supported())
}

/// A plain `eprintln!` collides with the spinner's steady-tick redraw; `hook_line` routes through `suspend` so the log lands cleanly above the still-spinning step.
static ACTIVE_SPINNER: Mutex<Option<ProgressBar>> = Mutex::new(None);

fn set_active_spinner(pb: Option<ProgressBar>) {
    *ACTIVE_SPINNER.lock().unwrap() = pb;
}

fn emit_around_spinner(f: impl FnOnce()) {
    // Drop the mutex before suspending so a reentrant print can't deadlock on it.
    let pb = ACTIVE_SPINNER.lock().unwrap().clone();
    match pb {
        Some(pb) => pb.suspend(f),
        None => f(),
    }
}

pub fn header(op: &str, args: &str) {
    if tty() {
        eprintln!("{} {} {}", style("◐").cyan(), style(op).bold(), args);
    } else {
        eprintln!("{op}: {args}");
    }
}

/// Begin a step; a `Drop` without a terminal call (e.g. `?` short-circuit) clears the spinner and emits an aborted line.
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
        // So `hook_line` streamed under this step suspends it.
        set_active_spinner(Some(pb.clone()));
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
    /// Deregisters so streamed output after this step no longer tries to suspend a bar that's gone.
    fn clear_spinner(&mut self) {
        if let Some(pb) = self.spinner.take() {
            pb.finish_and_clear();
        }
        set_active_spinner(None);
    }

    pub fn ok(mut self) {
        self.finished = true;
        self.finish_ok_inner(None);
    }

    pub fn ok_with(mut self, suffix: &str) {
        self.finished = true;
        self.finish_ok_inner(Some(suffix.to_string()));
    }

    pub fn fail(mut self, reason: &str) {
        self.finished = true;
        self.clear_spinner();
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
        self.clear_spinner();
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
        self.clear_spinner();
        // Aborted line so a `?` upstream doesn't leave a silent gap between the header and the propagated error.
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

/// `stderr` flag is unused — v1 gives it no distinct styling per spec-output-formatting §"Hook-streamed line".
pub fn hook_line(tag: &str, line: &str, _stderr: bool) {
    let cleaned = strip_control(line);
    emit_around_spinner(|| {
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
    });
}

pub fn skip(tag: &str, reason: &str) {
    if tty() {
        eprintln!(
            "  {} {} {}",
            style(pad_tag(tag)).green().bold(),
            style("·").dim(),
            style(reason).dim(),
        );
    } else {
        eprintln!("  {}  skip  {}", pad_tag(tag), reason);
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

pub fn warn(title: &str, items: &[String]) {
    if tty() {
        eprintln!("  {} {}", style("⚠").yellow().bold(), style(title).yellow());
    } else {
        eprintln!("  warn: {title}");
    }
    for item in items {
        if tty() {
            eprintln!("        {}", style(item).dim());
        } else {
            eprintln!("        {item}");
        }
    }
}

pub fn summary_fail(reason: &str) {
    if tty() {
        eprintln!("{} {}", style("✗").red().bold(), style(reason).red());
    } else {
        eprintln!("FAIL: {reason}");
    }
}

pub struct ListRow {
    pub repo: String,
    pub dirty: bool,
    /// `None` when the `status` hook is missing, timed out, or failed.
    pub status: Option<String>,
    pub note: Option<String>,
    /// A container repo's status column only summarizes ("6 up"), so individual dialable ports would otherwise be invisible.
    pub services: Vec<(String, u16)>,
}

pub struct ListGroup {
    pub task: String,
    /// Empty for the unmanaged group.
    pub path: String,
    pub rows: Vec<ListRow>,
}

pub fn list_empty() {
    if tty() {
        eprintln!("\n{}", style("no worktrees").dim());
    } else {
        eprintln!("\nno worktrees");
    }
}

pub fn list_table(groups: &[ListGroup]) {
    let repo_w = groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .map(|r| r.repo.len())
        .max()
        .unwrap_or(0);

    // Skip the column when nothing reports one, else every row is a bare dash.
    let any_status = groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .any(|r| r.status.is_some());
    let dash = if tty() { "—" } else { "-" };

    eprintln!();
    for group in groups {
        if tty() {
            let mut header = style(&group.task).bold().to_string();
            if !group.path.is_empty() {
                header.push_str(&format!("  {}", style(&group.path).dim()));
            }
            eprintln!("  {header}");
        } else {
            let sep = if group.path.is_empty() { "" } else { "  " };
            eprintln!("  {}{sep}{}", group.task, group.path);
        }
        for row in &group.rows {
            let repo = format!("{:<width$}", row.repo, width = repo_w);
            let dirty_txt = if row.dirty { "dirty" } else { "clean" };
            if tty() {
                let dirty = if row.dirty {
                    style(format!("{dirty_txt:<5}")).yellow()
                } else {
                    style(format!("{dirty_txt:<5}")).green()
                };
                let mut line = format!("    {} {}", style(repo).green().bold(), dirty);
                if any_status {
                    let status = row.status.as_deref().unwrap_or(dash);
                    line.push(' ');
                    line.push_str(&style(status).dim().to_string());
                }
                if let Some(note) = &row.note {
                    line.push(' ');
                    line.push_str(&style(format!("({note})")).dim().to_string());
                }
                eprintln!("{line}");
            } else {
                let mut line = format!("    {repo} {dirty_txt:<5}");
                if any_status {
                    line.push(' ');
                    line.push_str(row.status.as_deref().unwrap_or(dash));
                }
                if let Some(note) = &row.note {
                    line.push_str(&format!(" ({note})"));
                }
                eprintln!("{line}");
            }
            // Widths are per-row so one repo's long service name doesn't push another's ports out.
            let name_w = row.services.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
            for (name, port) in &row.services {
                let name_padded = format!("{name:<name_w$}");
                if tty() {
                    eprintln!("      {} {}", style(name_padded).dim(), style(format!(":{port}")).dim());
                } else {
                    eprintln!("      {name_padded} :{port}");
                }
            }
        }
        eprintln!();
    }
}

pub fn clean_verdict(task: &str, removal: bool, reason: &str, dry: bool) {
    let verb = if removal {
        if dry { "would remove" } else { "remove" }
    } else {
        "keep"
    };
    if tty() {
        let (mark, label) = if removal {
            (style("✓").green().to_string(), style(verb).green().to_string())
        } else {
            (style("·").dim().to_string(), style(verb).dim().to_string())
        };
        eprintln!(
            "  {} {} {} — {}",
            mark,
            style(task).bold(),
            label,
            style(reason).dim(),
        );
    } else {
        eprintln!("  {task}  {verb}  {reason}");
    }
}

pub fn clean_empty() {
    if tty() {
        eprintln!("\n{}", style("nothing to clean").dim());
    } else {
        eprintln!("\nnothing to clean");
    }
}

/// stdout is the one channel scripts consume (`ws config get`); everything human-facing goes to stderr, and this line is never styled.
pub fn value(text: &str) {
    println!("{text}");
}

/// Indented to the repo column (2), not 0 — column 0 belongs to the header and closing summary, and a note there read as a peer of the operation rather than a detail.
pub fn note(text: &str) {
    if tty() {
        eprintln!("  {}", style(text).dim());
    } else {
        eprintln!("  {text}");
    }
}

/// Column-aligned block for related facts (per-service port forwards); `tag` uses `step`'s repo column so a sweep's per-repo blocks stay distinguishable.
pub fn table(tag: &str, title: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    let w = key_width(rows);
    emit_around_spinner(|| {
        if tty() {
            eprintln!("  {} {}", style(pad_tag(tag)).green().bold(), style(title).dim());
            for (k, v) in rows {
                eprintln!("    {}  {}", style(pad_key(k, w)).dim(), style(v).dim().bold());
            }
        } else {
            eprintln!("  {}  {title}", pad_tag(tag));
            for (k, v) in rows {
                eprintln!("    {}  {}", pad_key(k, w), v);
            }
        }
    });
}

/// Counted in `chars` so a multi-byte service name doesn't over-pad.
fn key_width(rows: &[(String, String)]) -> usize {
    rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0)
}

/// Must be called on the raw string — styled input pads short because ANSI escapes count against the width specifier.
fn pad_key(k: &str, w: usize) -> String {
    format!("{k:<w$}")
}

pub fn nudge(text: &str) {
    if tty() {
        eprintln!("\n{}", style(format!("Next: {text}")).dim().italic());
    } else {
        eprintln!("\nnext: {text}");
    }
}

pub fn status_line(ok: bool, label: &str, detail: &str) {
    if tty() {
        let (mark, lab) = if ok {
            (style("✓").green().to_string(), style(label).bold().to_string())
        } else {
            (style("✗").yellow().bold().to_string(), style(label).bold().to_string())
        };
        eprintln!("  {mark} {lab} {}", style(format!("— {detail}")).dim());
    } else {
        eprintln!("  {} {label} — {detail}", if ok { "ok" } else { "MISSING" });
    }
}

pub fn command_block(commands: &[String]) {
    for c in commands {
        if tty() {
            eprintln!("      {}", style(c).cyan());
        } else {
            eprintln!("      {c}");
        }
    }
}

/// Exposed so a caller can skip offering a choice it already knows can't be answered, rather than printing a dead prompt.
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Returns `None` on non-tty or EOF so the caller can fall back to print-and-bail rather than block; the prompt has no trailing newline so the answer echoes on the same line.
pub fn ask(prompt: &str) -> Option<String> {
    use std::io::Write;
    if !interactive() {
        return None;
    }
    if tty() {
        eprint!("  {} {} ", style("?").cyan().bold(), style(prompt).bold());
    } else {
        eprint!("  {prompt} ");
    }
    std::io::stderr().flush().ok();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim().to_string()),
        Err(_) => None,
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

/// Stops hooks from clearing the terminal, faking op status lines, or otherwise scribbling on ws's own output.
fn strip_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                match chars.next() {
                    Some('[') => {
                        // CSI ends on a byte in 0x40..=0x7e.
                        for c in chars.by_ref() {
                            if matches!(c, '\x40'..='\x7e') {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC ends on BEL or ST (ESC \).
                        let mut prev = '\0';
                        for c in chars.by_ref() {
                            if c == '\x07' || (prev == '\x1b' && c == '\\') {
                                break;
                            }
                            prev = c;
                        }
                    }
                    Some(_) => {
                        // Single-byte escape (e.g. ESC c reset) — drop the byte.
                    }
                    None => {}
                }
            }
            // C0 controls, but tab and newline are kept.
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
    fn table_values_all_start_in_the_same_column() {
        let rows: Vec<(String, String)> = [("db", "localhost:20000"), ("elasticsearch", "localhost:20002")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let w = key_width(&rows);
        assert_eq!(w, "elasticsearch".len());

        let cols: Vec<usize> = rows
            .iter()
            .map(|(k, v)| format!("    {}  {}", pad_key(k, w), v).find("localhost").unwrap())
            .collect();
        assert_eq!(cols[0], cols[1], "value column must not depend on key length");
        assert_eq!(pad_key("elasticsearch", w), "elasticsearch");
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
