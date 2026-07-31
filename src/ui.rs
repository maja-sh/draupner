//! Visual language for user-facing output. See `spec-output-formatting.md`.
//!
//! Op code never calls `println!`/`eprintln!` directly. Every op uses these
//! helpers so tty detection, color palette, spinners, and non-tty fallback
//! all live in one place.

use console::{style, Term};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Left-pad width for the step-line tag column. Widest tag currently in use is
/// `"skills"` (6). Longer tags overflow naturally.
const TAG_WIDTH: usize = 6;

fn tty() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| Term::stderr().features().colors_supported())
}

/// The spinner currently animating, if any. A streamed line (`hook_line`) printed
/// with a plain `eprintln!` collides with the spinner's steady-tick redraw on the
/// same terminal row; emitting it *through* the spinner (`suspend`) makes indicatif
/// clear its line first, so the log lands cleanly above the still-spinning step.
static ACTIVE_SPINNER: Mutex<Option<ProgressBar>> = Mutex::new(None);

fn set_active_spinner(pb: Option<ProgressBar>) {
    *ACTIVE_SPINNER.lock().unwrap() = pb;
}

/// Run `f` (which prints one line) with the active spinner suspended, so the line
/// lands above the spinner instead of fighting its redraw. A no-op wrapper when no
/// spinner is live (non-tty, or between steps).
fn emit_around_spinner(f: impl FnOnce()) {
    // Clone the handle out and drop the lock before suspending, so a reentrant
    // print (there is none today) couldn't deadlock on the mutex.
    let pb = ACTIVE_SPINNER.lock().unwrap().clone();
    match pb {
        Some(pb) => pb.suspend(f),
        None => f(),
    }
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
        // Register it so `hook_line` streamed under this step suspends it.
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
    /// Finish and clear the spinner, and deregister it so streamed output after
    /// this step no longer suspends a bar that's gone.
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

/// A dim, subordinate note that a repo was skipped for a step (e.g. it has a
/// worktree but no hook for this event). Not a failure — just visible enough
/// that the user knows nothing ran for that repo.
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

/// Up-front warning block — a `⚠` title followed by indented detail lines.
/// Used by `ws rm` to surface dirty / unpushed state before teardown.
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

/// One repo's row within a `ws list` task group.
pub struct ListRow {
    pub repo: String,
    pub dirty: bool,
    /// Free-form run-state string emitted by the `status` hook (first line),
    /// or `None` when there's no hook / it timed out / it failed.
    pub status: Option<String>,
    /// Extra tag (`detached`, `unmanaged`, …). Rendered dim after the status.
    pub note: Option<String>,
}

/// A task grouping for `ws list` — one branch, one or more repo worktrees.
pub struct ListGroup {
    /// Display label — the branch name (`feat/foo`) or the sanitized task dir.
    pub task: String,
    /// The task dir holding this branch's worktrees (`<root>/.worktrees/<task>`),
    /// so a reader can find the files; empty for the unmanaged group.
    pub path: String,
    pub rows: Vec<ListRow>,
}

/// `ws list` with no worktrees found. Not an error — informational.
pub fn list_empty() {
    if tty() {
        eprintln!("\n{}", style("no worktrees").dim());
    } else {
        eprintln!("\nno worktrees");
    }
}

/// Render the `ws list` table: task groups separated by blank lines, each with
/// aligned repo / dirty / status columns.
pub fn list_table(groups: &[ListGroup]) {
    let repo_w = groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .map(|r| r.repo.len())
        .max()
        .unwrap_or(0);

    // Only show the status column when something actually reports one —
    // otherwise every row is a bare dash, which reads as noise.
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
        }
        eprintln!();
    }
}

/// One `ws clean` audit verdict. `removal` drives the color (green remove / dim
/// keep); `dry` renders a removal as "would remove" for the preview pass.
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

/// `ws clean` with no task worktrees to audit. Not an error — informational.
pub fn clean_empty() {
    if tty() {
        eprintln!("\n{}", style("nothing to clean").dim());
    } else {
        eprintln!("\nnothing to clean");
    }
}

/// A machine-readable value on stdout — the one channel scripts consume
/// (`ws config get`); everything human-facing goes to stderr via the helpers
/// above. No styling ever, tty or not.
pub fn value(text: &str) {
    println!("{text}");
}

/// A single dim, subordinate line to stderr. Used by `ws exec` to note where a
/// command is being dispatched (e.g. the remote host) without polluting the
/// command's own stdout.
///
/// Indented to the repo column (2), like `step` and `warn`. Column 0 belongs to
/// op-level lines — the header and the closing summary — and a note printed
/// there read as a peer of the operation itself rather than a detail of it,
/// leaving a long one flush against the edge while everything around it was
/// inset.
pub fn note(text: &str) {
    if tty() {
        eprintln!("  {}", style(text).dim());
    } else {
        eprintln!("  {text}");
    }
}

/// A titled, column-aligned detail block: a dim title at the repo column and
/// `key   value` rows inset under it.
///
/// For a set of related facts that would otherwise be N repetitive `note` lines
/// — the per-service port forwards, where every line shared a prefix and the
/// distinguishing part was two words wide. One line per fact is right; N
/// unaligned sentences are not. Padding is applied to the raw key before
/// styling, since ANSI escapes would otherwise be counted in the field width.
pub fn table(title: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    let w = key_width(rows);
    emit_around_spinner(|| {
        if tty() {
            eprintln!("  {}", style(title).dim());
            for (k, v) in rows {
                eprintln!("    {}  {}", style(pad_key(k, w)).dim(), style(v).dim().bold());
            }
        } else {
            eprintln!("  {title}");
            for (k, v) in rows {
                eprintln!("    {}  {}", pad_key(k, w), v);
            }
        }
    });
}

/// Width of a table's key column: the widest key, counted in `chars` so a
/// multi-byte service name doesn't over-pad.
fn key_width(rows: &[(String, String)]) -> usize {
    rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0)
}

/// Pad a key to the column width. Applied to the raw string, never a styled
/// one — `style()` emits ANSI escapes that a width specifier would count as
/// visible characters, so every coloured row would pad short by the escape
/// length and the value column would ragged out.
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

/// A readiness line for `ws setup`: `✓ label — detail` (green) when satisfied,
/// `✗ label — detail` (yellow) when a gap remains. Not a hard failure — a gap is
/// something the user is about to be walked through fixing.
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

/// Show an exact command block (for `ws setup`'s show-me / print-and-bail): each
/// line indented and, on a tty, rendered like inline code so it's copy-pasteable.
pub fn command_block(commands: &[String]) {
    for c in commands {
        if tty() {
            eprintln!("      {}", style(c).cyan());
        } else {
            eprintln!("      {c}");
        }
    }
}

/// Ask an interactive question and read a line from stdin. Returns `None` when
/// stdin isn't a terminal or hits EOF — the caller then falls back to a
/// non-interactive path (print-and-bail) rather than blocking. The prompt has no
/// trailing newline so the answer echoes on the same line.
pub fn ask(prompt: &str) -> Option<String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
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
        Ok(0) => None, // EOF
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
    fn table_values_all_start_in_the_same_column() {
        // The point of the block over N notes is that the values line up, so
        // assert the column rather than eyeballing a sample render.
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
        // The widest key is padded to exactly itself — no trailing filler.
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
