---
name: wsx-down
description: Stop a task's worktree environment — bring containers down, terminate file syncs, leave the worktree files intact. Symmetric counterpart to /wsx-up. Project-specific behavior lives in per-repo `down` hooks under `.wsx-hooks/`. Use when the user runs /wsx-down <branch>.
---

Invoke via bash:
`wsx down $ARGS`

Print `wsx`'s stdout/stderr verbatim. Do not summarize.
