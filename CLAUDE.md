# CLAUDE.md

Project rules for AI-assisted work in this repo.

## Comments

Write a comment when the code makes a choice a reader would otherwise undo. That
is the whole test.

- **Never restate the code.** If what it does needs explaining in prose, the
  names are wrong — fix those instead.
- **Never signpost structure.** No section banners, no `// 1. …` step markers. A
  file that needs a table of contents wants splitting, not annotating.
- **Explain why, and only when the why is non-obvious.** "This looks wrong and
  isn't" is the bar. A reason a competent reader would arrive at unaided does not
  need writing down.

**The story goes in the commit message; the comment gets one sentence.** An
incident is better recorded where `git blame` will hand it to whoever needs it,
in full, than pasted above the fix where every future reader pays for it. The
comment only needs enough to stop someone deleting the line.

Worth stating plainly because this repo got it wrong for a while: a 44%
comment-to-code ratio is not thoroughness, and "preserve every comment" is not a
safe default — it protects the load-bearing and the bloated equally.

## Commit messages

### Subject line

Format: `[scope:] imperative summary [(#PR)]`

- **Scope** — optional but conventional. Lowercase module/component name followed by a colon:
  `ops:`, `ui:`, `compat:`, `hooks:`, `skills:`, `config:`, `docs:`, `ci:`, `perf:`, `test:`.
  Skip the scope for cross-cutting changes (release bumps, wide refactors).
- **Verb** — imperative mood. `Add`, `Fix`, `Remove`, `Refactor`, `Improve`, `Bump`, `Restore`,
  `Reduce`, `Support`. Never past tense (`Added`, `Fixed`).
- **Summary** — concrete about what changed. Not `Fix bug`; `Fix off-by-one in hook line pager`.
- **No trailing period.**
- **PR reference** goes at the end as `(#NNN)` when there is one. Optional for direct-to-main
  commits (there's no PR yet).
- **Length** — aim for ≤ 72 chars; hard wall at ~90 including the PR ref.

### Body

Optional. Include one when the change deserves explanation — the "why," a design trade-off,
a subtle mechanism. Skip it for changes whose subject line already says everything.

Two acceptable shapes:

**Prose** (default for small-to-medium changes)

    One or two paragraphs. Explain the motivation, not the diff. Wrap around
    72 chars. Reference issues by GitHub keyword when closing (`Closes #12`,
    `Fixes #12`).

    Release Notes:
    - Short user-facing description, or `N/A` for internal-only.

**Structured** (larger features, non-trivial PRs)

    # Objective

    What problem this solves.

    ## Solution

    - Bullet points describing the approach.
    - One bullet per non-obvious decision.

    ## Testing

    How the change was verified. Unit tests, manual repro, benchmarks.

    Release Notes:
    - User-facing summary, or `N/A`.

The `Release Notes:` section is expected on every commit that reaches `main`. It drives
downstream changelog work; use `N/A` when the change is internal.

### Trailers

Standard git trailers at the very end of the message, blank line above:

    Closes #123
    Co-authored-by: Real Person <real@example.com>

**Do not add `Co-authored-by:` trailers for AI models.** Even when AI assistance materially
contributed to a commit, the trailer is for humans only.

### Examples

    compat: Shrink WS_COMPAT_MATRIX parser and add malformed-range tests

    ---

    ops: Send hook stderr through ui::hook_line same as stdout (#42)

    Previously stderr bypassed the gutter/indent and dumped raw. Route both
    streams through the same helper so the visual language is consistent.

    Release Notes:
    - Hook stderr now renders with the same gutter and dim styling as stdout.

    ---

    ui: Reduce cloning when rendering step lines

    ---

    # Objective
    `ws new` was failing on shared-repo workspaces because the hook lookup
    resolved paths relative to the workspace root instead of the config file's
    directory.

    ## Solution
    - Thread the config dir through `hooks::lookup`.
    - Extend the fixture builder to cover the `.ws/` layout.

    ## Testing
    - Added `hooks_lookup_uses_config_dir_in_shared_layout`.
    - `cargo nextest run` — all pass.

    Release Notes:
    - Fix `.ws-hooks/<repo>/<event>` discovery inside shared-repo workspaces.

    Closes #17
