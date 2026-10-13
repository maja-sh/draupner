---
name: wsx-context
description: Cwd-adaptive JSON wakeup for LLM agents — workspace facts, current task/repo derived from cwd, per-repo state, aggregate task health, and in-band `hints` (do/don't rules + enum vocab). Run this first when starting work in a ws workspace, or when unsure which task the cwd belongs to. Use when the user runs /wsx-context, asks to resume or work on a task, or the agent needs to reground.
---

Invoke via bash:
`wsx context`

The response is JSON on stdout. Treat it as the sole grounding for this session — do not run other `wsx` commands to compensate for information the response should carry.

## 1. Read `hints` first

The `hints` array is authoritative. It defines every enum's vocabulary (`task.state.kind`, `task.repos[].kind`, `is_running` semantics), lists the do/don't rules for operating in this workspace, and adds conditional guidance based on the response's state (e.g. remote workspace, stack not running, cwd outside any task). Follow every rule verbatim — do not paraphrase, reorder, or drop.

## 2. Ground on the payload

- `workspace.root` — absolute path to the workspace root.
- `current.task` — the branch (as originally typed, may contain `/`) of the enclosing worktree. `null` when the cwd is outside `.worktrees/`; ask the user which task to work in.
- `current.repo` — the repo whose subtree the cwd is in, or `null`.
- `task` — full detail on the current task. `null` when `current.task` is `null`.
- `task.state.kind` — aggregated `running` / `degraded` / `stopped` / `unknown`. The numerics (`healthy_repos`, `total_repos`, `any_running`) are the ground truth if the label is unclear.
- `task.repos[].worktree_absolute` — absolute path to that repo's worktree. Edit files there; do not `cd`.
- `task.repos[].exec_command_prefix` — command prefix for running anything inside that repo. Append a space and your command.
- `task.repos[].services[].routed_hostname` — the correct HTTPS hostname if `is_http_routed` is true. Use verbatim.
- `task.repos[].services[].allocated_host_port` — TCP host port for raw-TCP or backing services; `null` on a never-up worktree.
- `task.repos[].is_running` — `true` / `false` / `null`. `null` means the check could not complete — do NOT infer stopped.

## 3. Do not derive what the response tells you

- Do not compute hostnames from the branch/repo — `routed_hostname` already handled sanitization (a dotted `feat/v1.2` becomes `feat-v1-2`).
- Do not guess `is_running` from other signals.
- Do not run `wsx up`, `wsx down`, `wsx rm`, or `wsx new` without explicit user consent.

## 4. Report back briefly

Tell the user what you found — the task, the aggregate state, and any conditional hint the response emitted for this workspace (remote, stack not running, cwd outside a task). Keep it short so it's obvious the grounding landed and in what state.

## 5. Re-run when the state might have changed

`wsx context` is a snapshot. After the user runs `wsx up`, `wsx down`, or changes cwd, re-run it before acting on stale state.
