# Contributing

Thanks for your interest in `drup`! A few things to know before you open a PR.

## Build and test

```sh
cargo build
cargo test                    # or `cargo nextest run` if you have it
```

Tests run the real mechanisms — real git repos, real worktrees, real detached
processes — in temporary fixtures with `$HOME` redirected. A few are gated on
external binaries (`docker compose`, `lsof`, `python3`) and skip with a note
rather than failing when the binary isn't present.

CI runs on Linux today; the release build and gating tests run on macOS. If
you're adding behavior that touches platform-specific paths (routing, DNS,
process management), make sure the macOS story works.

## Style

This crate is deliberately hand-formatted — do not run `cargo fmt`. Match the
surrounding style; keep columns reasonable, keep diffs small.

See [`CLAUDE.md`](CLAUDE.md) for the comment style we aim for ("why, not what,
and only when the why is non-obvious") and the commit message conventions.

## Commits and pull requests

- One logical change per commit; keep the subject imperative and under ~72
  chars with an optional scope prefix (e.g. `ui:`, `ops:`, `docs:`).
- End each commit body with a `Release Notes:` section — either a short
  user-facing line or `N/A` for internal-only changes. The release workflow
  pulls these forward into the GitHub Release.
- PRs target `main`.
- Do **not** add `Co-authored-by:` trailers for AI assistants.

Full spec is in [`CLAUDE.md`](CLAUDE.md).

## Reporting bugs and ideas

Open an issue — there are templates for both bug reports and feature requests.
For security issues, see [`SECURITY.md`](SECURITY.md).
