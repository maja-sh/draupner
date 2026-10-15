---
name: drup-add
description: Add repo(s) to an existing task worktree in the current workspace directory. If the task's stack is up, bring it down first (`drup down <branch>`) — `drup add` refuses when any existing repo reports as running/degraded/crashed. Use when the user runs /drup-add with a comma-separated repo list and an existing branch name (e.g. `/drup-add <repo3> feat/existing-branch`).
---

Invoke via bash:
`drup add $ARGS`

Print `drup`'s stdout/stderr verbatim. Do not summarize.
