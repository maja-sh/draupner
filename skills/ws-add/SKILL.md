---
name: ws-add
description: Add repo(s) to an existing task worktree in the current workspace directory. If the task's stack is up, bring it down first (`ws down <branch>`) — `ws add` refuses when any existing repo reports as running/degraded/crashed. Use when the user runs /ws-add with a comma-separated repo list and an existing branch name (e.g. `/ws-add <repo3> feat/existing-branch`).
---

Invoke via bash:
`ws add $ARGS`

Print `ws`'s stdout/stderr verbatim. Do not summarize.
