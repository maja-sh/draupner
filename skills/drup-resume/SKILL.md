---
name: drup-resume
description: Resume work on a drup worktree by branch — locate it, confirm it exists, and load how to operate it (files, exec, hostnames) without changing directory. Use when the user runs /drup-resume <branch> or asks to resume or work on a branch's worktree.
---

Invoke via bash:
`drup show $ARGS --json`

The user wants to work on a `drup` worktree, named by its **branch** — the argument (`$ARGS`).

## 1. Ground on the response

Parse the output; it is the ground truth for this worktree. Its shape:

```
{
  "branch":            original branch name (may contain "/"),
  "workspace_root":    absolute path to the workspace root,
  "remote":            null (local workspace) or:
                        { "connection", "worktree_base_absolute" },
  "repos": [{
    "name":                repo name from .drup.json,
    "kind":                "host" | "container",
    "worktree_absolute":   absolute path to this repo's worktree,
    "is_running":          true | false | null (site unreachable),
    "services": [{
      "name":                  service name,
      "allocated_host_port":   integer or null (null = never brought up),
      "routed_hostname":       HTTP hostname or null,
      "is_http_routed":        true if reachable via https://<routed_hostname>
    }],
    "log_path_absolute":  absolute path to the host log (null for containers),
    "exec_command_prefix": "drup exec --branch <branch> <repo> --" — append your command
  }]
}
```

If the branch has no worktree, `drup show` exits 2 with `no worktrees for <branch>; run \`drup new\` first`. In that case, **stop**: tell the user the worktree doesn't exist and mention `drup new <repos> <branch>` if they meant to create it. Do not create it yourself.

## 2. Operate this worktree without `cd`-ing into it

For the rest of the session, this branch is your working context:

- **Edit files** at each repo's `worktree_absolute` path from the JSON.
- **Run commands** with the `exec_command_prefix` verbatim, appending your command with a space:
  ```
  <exec_command_prefix> <your command>
  ```
  Do **not** invoke `podman compose`, `docker`, `npm`, or build tools directly in the worktree — they miss the injected env and won't see the stack.
- **Read host logs** at `log_path_absolute`. Null means it's a container repo — use the container runtime's logs (`podman logs <container>` etc.).
- **Reach HTTP services** at `https://<routed_hostname>` from the service object (add the domain's port suffix if it isn't on :443). Do **not** re-derive hostnames from the branch or repo name; `drup show` already applied the correct sanitization (a dotted `feat/v1.2` collapses to `feat-v1-2`, not the string you'd guess).
- **Raw-TCP services** (`is_http_routed: false`) are reached at `localhost:<allocated_host_port>` — client-side forwards keep the ports identical to the server.
- **Lifecycle**: `drup show <branch>` for a fresh snapshot, `drup up <branch>` / `drup down <branch>` to bring the stack up or down. Do **not** auto-run `drup up`. If a repo is `is_running: false`, tell the user and let them decide.

## 3. Ground on the JSON, do not assume

- If a field is `null`, treat it as unknown. Do not fill it in from the branch or repo name.
- If a piece of information isn't in the JSON, the answer is "I don't know" — not a derived guess.
- If a value contradicts something you'd have derived (e.g. a hostname that doesn't match the branch), trust the JSON.

## 4. Report back

Briefly tell the user what you found — the repos in this worktree, whether each is running, and the routed hostname(s) if any — so it's obvious the resume landed and in what state.
