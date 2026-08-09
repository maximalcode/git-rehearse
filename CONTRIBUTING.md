# Contributing to git-rehearse

Thanks for taking a look. This is a small project with a simple workflow.

## Start with an issue

Every change starts as a GitHub issue — bug reports, features, refactors,
chores. Open one before you write code so the approach can be agreed on first;
that saves you from building something that gets turned down at review.

## Setup

You need the **pinned Rust toolchain** (rustup reads it from CI; currently
1.97.1) and **git on your PATH** (the tool shells out to it).

```bash
cargo build
cargo test --locked
```

## Before you open a pull request

```bash
cargo fmt --check
RUSTFLAGS="-Dwarnings" cargo clippy --all-targets --locked
cargo test --locked
```

CI runs exactly these (plus `cargo deny check advisories bans` and the
cross-language [maxi-quality](https://github.com/maximalcode/maxi-quality)
Layer 2 scan), so running them locally just saves you a round trip. Lint
configuration (`rustfmt.toml`, `deny.toml`, the `[lints]` block in
`Cargo.toml`) is adopted from maxi-quality — fix findings rather than editing
the config; config changes belong upstream.

## Branches and pull requests

- `develop` is the working branch. Branch from it, and target your pull
  request at it.
- `main` is release-only. It moves through `develop` → `main` pull requests,
  and a release is a version tag on top.
- Name branches `<type>/<issue-number>-<slug>`, e.g. `feat/12-sandbox-clone`,
  with type one of `feat` / `fix` / `refactor` / `docs` / `infra` / `chore`.
- Put `Closes #12` in the pull request body so the issue closes on merge.

## Project conventions

Standing constraints worth knowing before you add code — the full versions
with rationale live in [SCOPE.md](SCOPE.md) ("Design principles"), and they are
not up for casual relitigation:

- **Real git executes, always.** We never reimplement or approximate git
  semantics; the sandbox runs the user's actual `git` binary.
- **Apply = ref transplant, never re-run.** This one is inviolable.
- **The sandbox is disposable and inert** — no remotes, hooks off by default.
- **No telemetry, no network, no accounts.** Ever.
- **`unsafe` is forbidden** at the manifest level. This tool shells out to
  git; it needs none.
- Pure logic (ref analysis, report building, DAG walking) lives in its own
  module and gets unit tests; process-spawning code stays thin.
