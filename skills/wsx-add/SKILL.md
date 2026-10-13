---
name: wsx-add
description: Add repo(s) to an existing task worktree in the current workspace directory. If the task's stack is up, bring it down first (`wsx down <branch>`) — `wsx add` refuses when any existing repo reports as running/degraded/crashed. Use when the user runs /wsx-add with a comma-separated repo list and an existing branch name (e.g. `/wsx-add <repo3> feat/existing-branch`).
---

Invoke via bash:
`wsx add $ARGS`

Print `wsx`'s stdout/stderr verbatim. Do not summarize.
