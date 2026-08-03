//! Visual language for user-facing output. Op code never calls
//! `println!`/`eprintln!` directly — tty detection, palette, spinners, and the
//! non-tty fallback all live here.

use console::{style, Term};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Width of the widest tag in use (`skills`); longer tags overflow rather than
/// truncate.
const TAG_WIDTH: usize = 6;

fn tty() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| Term::stderr().features().colors_supported())
}

/// The spinner currently animating, if any. A plain `eprintln!` collides with the
/// spinner's steady-tick redraw on the same terminal row, so every line-emitting
/// helper that can run under a live step goes through `emit_around_spinner`.
static ACTIVE_SPINNER: Mutex<Option<ProgressBar>> = Mutex::new(None);

fn set_active_spinner(pb: Option<ProgressBar>) {
    *ACTIVE_SPINNER.lock().unwrap() = pb;
}

fn emit_around_spinner(f: impl FnOnce()) {
    // Clone the handle out so the lock is dropped before `f` runs; holding it
    // across a reentrant print would deadlock.
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
    fn clear_spinner(&mut self) {
        if let Some(pb) = self.spinner.take() {
            pb.finish_and_clear();
        }
        set_active_spinner(None);
    }

    pub fn ok(mut self) {
        self.emit_ok(None);
    }

    pub fn ok_with(mut self, suffix: &str) {
        self.emit_ok(Some(suffix));
    }

    pub fn fail(mut self, reason: &str) {
        self.emit_fail(Some(reason));
    }

    fn emit_ok(&mut self, suffix: Option<&str>) {
        self.finished = true;
        // Read the clock before clearing the spinner so the reported timing is
        // the step's, not the step's plus the terminal repaint.
        let timing = format_timing(self.start.elapsed());
        self.clear_spinner();
        eprintln!("{}", self.ok_line(suffix, &timing));
    }

    /// Shared by the explicit `fail` and the aborted-step `Drop` so the two can't
    /// drift apart; `reason` is the only difference between them.
    fn emit_fail(&mut self, reason: Option<&str>) {
        self.finished = true;
        self.clear_spinner();
        eprintln!("{}", self.fail_line(reason));
    }

    fn ok_line(&self, suffix: Option<&str>, timing: &str) -> String {
        let mut line = if tty() {
            format!(
                "  {} {} {}",
                style(pad_tag(&self.tag)).green().bold(),
                style("✓").green(),
                self.label,
            )
        } else {
            format!("  {}  ok  {}", pad_tag(&self.tag), self.label)
        };
        if let Some(s) = suffix {
            line.push(' ');
            line.push_str(s);
        }
        if tty() {
            line.push(' ');
            line.push_str(&style(format!("({timing})")).dim().to_string());
        } else {
            line.push_str(&format!(" [{timing}]"));
        }
        line
    }

    fn fail_line(&self, reason: Option<&str>) -> String {
        let mut line = if tty() {
            format!(
                "  {} {} {}",
                style(pad_tag(&self.tag)).green().bold(),
                style("✗").red().bold(),
                self.label,
            )
        } else {
            format!("  {}  FAIL  {}", pad_tag(&self.tag), self.label)
        };
        if let Some(r) = reason {
            if tty() {
                line.push_str(&format!(" — {}", style(r).red()));
            } else {
                line.push_str(&format!(" — {r}"));
            }
        }
        line
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Usually a `?` upstream: emit an aborted line so the user isn't left with
        // a half-drawn spinner and a silent gap before the propagated error.
        self.emit_fail(None);
    }
}

/// Hook stderr is styled identically to stdout by design — one channel, one look.
pub fn hook_line(tag: &str, line: &str) {
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
    emit_around_spinner(|| {
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
    });
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
    emit_around_spinner(|| {
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
    });
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
    /// First line of the `status` hook's output; `None` covers no hook, timeout
    /// and failure alike.
    pub status: Option<String>,
    /// Extra tag (`detached`, `unmanaged`, …).
    pub note: Option<String>,
}

pub struct ListGroup {
    /// The branch name (`feat/foo`), or the sanitized task dir when there isn't one.
    pub task: String,
    /// `<root>/.worktrees/<task>`; empty for the unmanaged group.
    pub path: String,
    pub rows: Vec<ListRow>,
}

/// Shared by every op whose enumeration came up empty; named for the shape, not
/// the op, so the next op reuses it instead of adding a sibling.
pub fn empty(msg: &str) {
    if tty() {
        eprintln!("\n{}", style(msg).dim());
    } else {
        eprintln!("\n{msg}");
    }
}

pub fn list_table(groups: &[ListGroup]) {
    let repo_w = col_width(groups.iter().flat_map(|g| g.rows.iter()).map(|r| r.repo.as_str()));

    // Suppress the column entirely when nothing reports a status; otherwise every
    // row is a bare dash, which reads as noise.
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
            let repo = pad_key(&row.repo, repo_w);
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

/// The one channel scripts consume, hence stdout and no styling ever, tty or not;
/// everything human-facing goes to stderr via the helpers above.
pub fn value(text: &str) {
    println!("{text}");
}

/// Indented to the repo column, not column 0: column 0 belongs to op-level lines
/// (the header and the closing summary), and a note printed there reads as a peer
/// of the operation rather than a detail of it.
pub fn note(text: &str) {
    emit_around_spinner(|| {
        if tty() {
            eprintln!("  {}", style(text).dim());
        } else {
            eprintln!("  {text}");
        }
    });
}

/// A column-aligned block for related facts that would otherwise be N repetitive
/// `note` lines. The title carries a `tag` in `step`'s repo column because a sweep
/// emits one block per repo, and untagged titles are indistinguishable.
pub fn table(tag: &str, title: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    let w = col_width(rows.iter().map(|(k, _)| k.as_str()));
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

/// Counted in `chars`, not bytes: `pad_key`'s `{:<w$}` fill counts chars, so a
/// byte-derived width would ragged out every row sharing a column with a
/// non-ASCII entry.
fn col_width<'a>(items: impl Iterator<Item = &'a str>) -> usize {
    items.map(|s| s.chars().count()).max().unwrap_or(0)
}

/// Must be applied to the raw string, never a styled one: a width specifier counts
/// `style()`'s ANSI escapes as visible characters, so a coloured row pads short by
/// the escape length.
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

/// A gap renders yellow rather than red: it isn't a failure, it's something the
/// user is about to be walked through fixing.
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

/// [`ask`] tests this itself; it is public so a caller can decide not to *offer*
/// something at all rather than print a question it knows can't be answered.
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// `None` on a non-terminal stdin or EOF, so the caller falls back to its
/// print-and-bail path instead of blocking forever. The prompt is deliberately
/// newline-free so the answer echoes on the same line.
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
        Ok(0) => None, // EOF
        Ok(_) => Some(line.trim().to_string()),
        Err(_) => None,
    }
}

pub fn first_nonempty_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|l| !l.is_empty())
}

fn pad_tag(tag: &str) -> String {
    pad_key(tag, TAG_WIDTH)
}

fn format_timing(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}s", secs as u64)
    }
}

/// Hook output is untrusted: left intact, an escape sequence could clear the
/// terminal, set its title, or forge ws's own status lines.
fn strip_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                match chars.next() {
                    Some('[') => {
                        // CSI: parameters, then a final byte in 0x40..=0x7e.
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
                        // Single-byte escape (e.g. ESC c reset) — drop it.
                    }
                    None => {}
                }
            }
            // C0 controls, less the tab (\x09) and newline (\x0a) the gap preserves.
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
        let w = col_width(rows.iter().map(|(k, _)| k.as_str()));
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

    /// Pre-marked `finished` so `Drop` stays quiet while a test inspects the
    /// lines this would have rendered.
    fn quiet_step(tag: &str, label: &str) -> Step {
        Step {
            tag: tag.to_string(),
            label: label.to_string(),
            start: Instant::now(),
            spinner: None,
            finished: true,
        }
    }

    #[test]
    fn aborted_step_renders_the_failure_line_minus_the_reason() {
        let s = quiet_step("api", "up");
        let aborted = s.fail_line(None);
        let reported = s.fail_line(Some("exit 1"));
        assert!(
            reported.starts_with(&aborted),
            "reported failure must extend the aborted line, got {reported:?} vs {aborted:?}"
        );
        assert!(reported.contains("exit 1"));
        assert!(!aborted.contains('—'), "no dangling separator without a reason");
    }

    #[test]
    fn ok_suffix_is_inserted_before_the_timing() {
        let s = quiet_step("api", "up");
        let bare = s.ok_line(None, "0.4s");
        let with_suffix = s.ok_line(Some("2 repos"), "0.4s");
        assert!(with_suffix.contains("2 repos"));
        // Removing the suffix must reproduce the plain line exactly: it belongs
        // between label and timing, never after the timing.
        assert_eq!(with_suffix.replacen(" 2 repos", "", 1), bare);
    }

    #[test]
    fn column_width_counts_chars_not_bytes() {
        // `æøå` is 6 bytes but 3 columns wide.
        let w = col_width(["api", "æøå"].into_iter());
        assert_eq!(w, 3);
        assert_eq!(pad_key("api", w), "api");
        assert_eq!(pad_key("æøå", w), "æøå");
    }

    #[test]
    fn pad_tag_pads_short_and_leaves_long_alone() {
        assert_eq!(pad_tag("cfg"), "cfg   ");
        assert_eq!(pad_tag("skills"), "skills");
        assert_eq!(pad_tag("post-create"), "post-create");
    }
}
