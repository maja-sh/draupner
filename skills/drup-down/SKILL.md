---
name: drup-down
description: Stop a task's worktree environment — bring containers down, terminate file syncs, leave the worktree files intact. Symmetric counterpart to /drup-up. Project-specific behavior lives in per-repo `down` hooks under `.drup-hooks/`. Use when the user runs /drup-down <branch>.
---

Invoke via bash:
`drup down $ARGS`

Print `drup`'s stdout/stderr verbatim. Do not summarize.
