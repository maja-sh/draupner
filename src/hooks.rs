//! Per-repo hook runner. Streams stdout + stderr through `ui::hook_line`
//! so hook output lands with the correct gutter/indent instead of dumping
//! raw to the terminal. See `spec-output-formatting.md` for the visual language.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use crate::ui;

#[allow(dead_code)]
pub fn run(workspace_root: &Path, hook_rel: &str, worktree_path: &Path) -> Result<()> {
    let hook = workspace_root.join(hook_rel);
    let hook_name = short_name(&hook);

    let mut child = Command::new("bash")
        .arg(&hook)
        .arg(worktree_path)
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn bash {}", hook.display()))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let name_out = hook_name.clone();
    let name_err = hook_name.clone();

    let out_h = thread::spawn(move || pipe(stdout, &name_out, false));
    let err_h = thread::spawn(move || pipe(stderr, &name_err, true));

    let status = child.wait()?;
    let _ = out_h.join();
    let _ = err_h.join();

    if !status.success() {
        anyhow::bail!("{hook_rel} exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

fn pipe<R: std::io::Read>(r: R, tag: &str, stderr: bool) {
    for line in BufReader::new(r).lines().map_while(std::io::Result::ok) {
        ui::hook_line(tag, &line, stderr);
    }
}

fn short_name(p: &Path) -> String {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("hook").to_string()
}
