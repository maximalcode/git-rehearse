# CLAUDE.md — git-rehearse

Instructions for any AI session working in this repo. Read this before touching
anything. [SCOPE.md](SCOPE.md) is the authoritative plan — read it before
implementing anything, and do not silently deviate from it.

## 1. Identity rail (hard rule)

Every commit in this repo is authored as the **maximalcode** GitHub user —
never the personal/global identity.

**Before the first commit of every session, verify:**

```bash
git config user.name
```

It MUST print `maximalcode`. If it prints anything else, or nothing at all:
**STOP.** Do not commit. Fix it first:

```bash
git config user.name maximalcode
git config user.email 213183497+maximalcode@users.noreply.github.com
```

Repo-local only — never `--global`. `gh auth status` must show `maximalcode`
as the active account before any `gh` operation that writes.

## 2. This repo is public

Everything pushed here is public history permanently — GitHub keeps PR diffs
and unreachable refs forever. No secrets, no local paths, no references to
private repos, in any commit, ever. The pre-commit hook runs gitleaks on the
staged index; CI is the real gate.

## 3. How we work: issues first

Every unit of work starts as a GitHub issue. The loop:

1. Pick an issue (or file one: `gh issue create`).
2. Branch from `develop`: `<type>/<issue-number>-<slug>`, type one of
   `feat` / `fix` / `refactor` / `docs` / `infra` / `chore`.
3. Build it. Reference the issue in commit messages.
4. Run the checks below — same ones CI runs.
5. PR targeting `develop` with `Closes #N` in the body.

`main` is release-only (`develop` → `main` PRs; a release is a `v*` tag on
top). `main` requires a PR plus green checks; `develop` blocks force-push and
deletion, direct pushes are fine there.

## 4. Commands

```bash
cargo build
cargo test --locked
cargo fmt --check
RUSTFLAGS="-Dwarnings" cargo clippy --all-targets --locked
cargo deny check advisories bans   # needs cargo-deny; CI runs it regardless
```

Quality config (`rustfmt.toml`, `deny.toml`, `[lints]` in `Cargo.toml`) is
adopted from [maxi-quality](https://github.com/maximalcode/maxi-quality) — fix
findings, don't edit the config here. Re-adopt with its `adopt.sh` rather than
hand-editing; drift from the baseline is the failure mode that repo exists to
prevent.

## 5. Design principles (from SCOPE.md — do not relitigate mid-task)

1. **Real git executes, always.** Never reimplement or approximate git
   semantics. The sandbox runs the user's actual `git` binary with the user's
   actual config.
2. **Apply = ref transplant, never re-run.** Applying fetches the sandbox's
   objects and atomically updates refs to the rehearsed SHAs. **This principle
   is inviolable** — the day apply re-runs a command in the real repo, the
   tool's one guarantee dies.
3. **The sandbox is disposable and inert.** Remotes stripped at creation,
   hooks disabled by default, lives under the user cache dir, auto-pruned.
4. **Zero telemetry, zero network, zero spend.** The tool never phones
   anywhere; CI stays on the GitHub Actions free tier.
5. **Refuse loudly rather than guess.** Refs moved between rehearse and apply
   → refuse. Submodules/LFS/worktrees/shallow (v1) → refuse with an
   explanation, exit code 4. (A dirty worktree was on this list until #59; it
   is now carried through the rehearsal, and what gets refused is a worktree
   that no longer holds what was carried. See SCOPE.md's v1.x item 2.)

## 6. Code conventions

- `unsafe_code` is forbidden at the manifest level. This tool needs none.
- Pure logic (ref analysis, DAG walk, report building) in its own modules with
  unit tests; process-spawning code stays thin.
- Integration tests build git fixture repos in code — never check in `.git`
  directories.
- Exit codes are API from v0.1 on (0 clean / 2 conflicts / 3 failed /
  4 refused / 1 internal). Don't burn them on other meanings.

<!-- BEGIN maxi-quality agent-guard sha256:41659a1def91ce97 -->

## The gate, and how a session ends

This repo's quality baseline is enforced by two hooks and one deny rule in
`.claude/settings.json`. They are not advice — they refuse.

**Run the gate through the recorder, not directly:**

```bash
"$HOME/.local/bin/quality-runtime" record-gate --root "${CLAUDE_PROJECT_DIR}" --gate
```

`--gate` runs the command this repo declares in `.claude/agent-guard.json`,
whole and through one shell, so a gate written as `a && b` is recorded as a
gate rather than as its first half. It passes the gate's exit code straight
through. (`-- <command>` still works for an ad-hoc run, and is what you want
when the thing you are running is not the declared gate.)

A session cannot end while the working tree holds changes the gate has not
seen. If it refuses, the message says which of the four cases you are in: never
ran, ran and failed, ran something that was not this repo's gate, or ran against
different content.

**Do not write `.claude/agent-guard-receipt.json` by hand.** The `Edit` tool is
refused on it — that is a deny rule in `.claude/settings.json`, not advice. A
shell command still reaches the file, and nothing downstream can tell: it is
the gate's own input, so a hand-written one passes. It is the single action
here that turns a guard into a lie.

**Do not pass `--no-verify` to `git commit` or `git push`.** That is refused
too. It switches off this repo's commit hook, which is the last check before
content the gate has not seen becomes a commit. If the hook is failing for a
reason that is not your change, say so — do not route around it.


<!-- END maxi-quality agent-guard -->
