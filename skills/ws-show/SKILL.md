---
name: ws-show
description: Focused per-worktree status endpoint — services, ports, hostnames, log paths, exec syntax for one branch. Prefer over /ws-list when the question is about one specific worktree. Use when the user runs /ws-show <branch> or asks about a specific worktree's state.
---

Invoke via bash:
`ws show $ARGS`

Print `ws`'s stdout/stderr verbatim. Do not summarize.

For machine-readable output, pass `--json`; run `ws show --describe` for the JSON schema.
