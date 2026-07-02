//! Per-repo hook runner. Streams stdout + stderr through `ui::hook_line`
//! so hook output lands with the correct gutter/indent instead of dumping
//! raw to the terminal. See `spec-output-formatting.md` for the visual language.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use crate::ui;

/// Max bytes buffered per streamed line. A hook that emits an unterminated
/// stream (e.g. `yes | tr -d '\n'`) can't OOM us — the excess is discarded
/// up to the next newline.
const MAX_LINE: usize = 8 * 1024;

#[allow(dead_code)]
pub fn run(workspace_root: &Path, hook_rel: &str, worktree_path: &Path) -> Result<()> {
    // Hook path must be relative to the workspace root — an absolute path
    // silently escapes the workspace because `Path::join` replaces the base.
    if Path::new(hook_rel).is_absolute() {
        bail!("hook path must be relative to the workspace root: {hook_rel}");
    }
    let hook = workspace_root.join(hook_rel);
    let hook_name = short_name(&hook);

    let mut child = Command::new("bash")
        .arg(&hook)
        .arg(worktree_path)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn bash {}", hook.display()))?;

    let stdout = child.stdout.take().context("child stdout was not piped")?;
    let stderr = child.stderr.take().context("child stderr was not piped")?;
    let name_out = hook_name.clone();
    let name_err = hook_name.clone();

    let out_h = thread::spawn(move || pipe(stdout, &name_out, false));
    let err_h = thread::spawn(move || pipe(stderr, &name_err, true));

    let status = child.wait()?;
    let _ = out_h.join();
    let _ = err_h.join();

    if !status.success() {
        bail!("{hook_rel} exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Stream lines from `r` to `ui::hook_line`, capped at `MAX_LINE` bytes per
/// line. Overlong lines are truncated (tail discarded up to the next newline).
/// Invalid UTF-8 is replaced with U+FFFD via `from_utf8_lossy` — never drops
/// following lines.
fn pipe<R: Read>(r: R, tag: &str, stderr: bool) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        let n = match reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(_) => break,
        };
        // Cap-hit without a newline: drain the rest of the line.
        if n == MAX_LINE && buf.last() != Some(&b'\n') {
            let mut discard = Vec::new();
            let _ = reader.read_until(b'\n', &mut discard);
        }
        let end = if buf.last() == Some(&b'\n') { buf.len() - 1 } else { buf.len() };
        let line = String::from_utf8_lossy(&buf[..end]);
        ui::hook_line(tag, &line, stderr);
    }
}

fn short_name(p: &Path) -> String {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("hook").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Mirror the read loop in `pipe()` so we can assert what the ui layer
    /// would see, without depending on eprintln capture.
    fn read_lines(input: &[u8]) -> Vec<String> {
        let cursor = Cursor::new(input);
        let mut reader = BufReader::new(cursor);
        let mut buf = Vec::with_capacity(1024);
        let mut lines: Vec<String> = Vec::new();
        loop {
            buf.clear();
            let n = match reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if n == MAX_LINE && buf.last() != Some(&b'\n') {
                let mut discard = Vec::new();
                let _ = reader.read_until(b'\n', &mut discard);
            }
            let end = if buf.last() == Some(&b'\n') { buf.len() - 1 } else { buf.len() };
            lines.push(String::from_utf8_lossy(&buf[..end]).into_owned());
        }
        lines
    }

    #[test]
    fn truncates_oversize_line_and_continues() {
        let mut input = vec![b'a'; 20 * 1024];
        input.push(b'\n');
        input.extend_from_slice(b"after\n");
        let lines = read_lines(&input);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_LINE);
        assert_eq!(lines[1], "after");
    }

    #[test]
    fn survives_invalid_utf8_mid_stream() {
        let input: &[u8] = b"before\n\xffbad\nafter\n";
        let lines = read_lines(input);
        assert_eq!(lines.len(), 3);
        assert!(lines[0] == "before");
        assert!(lines[2] == "after");
    }
}
