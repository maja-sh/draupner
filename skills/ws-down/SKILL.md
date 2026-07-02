---
name: ws-down
description: Stop a task's worktree environment — bring containers down, terminate file syncs, leave the worktree files intact. Symmetric counterpart to /ws-up. Project-specific behavior lives in per-repo `down` hooks under `.ws-hooks/`. Use when the user runs /ws-down <branch>.
---

Invoke via bash:
`ws down $ARGS`

Print `ws`'s stdout/stderr verbatim. Do not summarize.
