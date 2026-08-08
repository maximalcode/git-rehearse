# git-rehearse

[![quality](https://github.com/maximalcode/git-rehearse/actions/workflows/quality.yml/badge.svg?branch=develop)](https://github.com/maximalcode/git-rehearse/actions/workflows/quality.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/maximalcode/git-rehearse/badge)](https://scorecard.dev/viewer/?uri=github.com/maximalcode/git-rehearse)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Rehearse dangerous git commands in a shadow clone of your real repo. See
exactly what would happen — then apply it or throw it away.**

```console
$ git rehearse rebase main
```

The real rebase runs in a disposable clone of your repository. You get the
before/after graph, the conflicts, and a warning if replaying your commits
quietly changed what they do. Then you choose: apply it, keep it, or throw it
away. Your repository is not touched until you say so.

> **Status: v0.1 is built, but not released yet.** The code below all works —
> it is what produced every terminal transcript on this page — and it is merged
> on `develop`. There is no tagged release and nothing on crates.io yet, so
> [install from source](#install) for now. [SCOPE.md](SCOPE.md) is the full
> plan, including the agent-facing v2 that is the real target.

## Install

No release is tagged yet, so the two paths that work today both build from
source. You need [Rust 1.97 or newer](https://rustup.rs) and `git` on your
`PATH`.

```bash
cargo install --git https://github.com/maximalcode/git-rehearse --branch develop
```

Or from a clone:

```bash
git clone https://github.com/maximalcode/git-rehearse && cd git-rehearse && cargo install --path .
```

Both install a binary called `git-rehearse`. Because git treats any
`git-<name>` on your `PATH` as a subcommand, that is all it takes for
`git rehearse …` to work — no alias, no config.

When v0.1 is tagged, the [releases page](https://github.com/maximalcode/git-rehearse/releases)
will carry prebuilt binaries for Linux, macOS and Windows with SHA-256 sums
beside them.

## A real session

Every transcript below is real captured output. The only thing edited is the
repository path, which is a stand-in for wherever you happen to be working.

A feature branch, and `main` has moved on. Rehearse the rebase and keep it:

```console
$ git rehearse --keep rebase main
Rebasing (1/1)Successfully rebased and updated refs/heads/feature.

rehearsed  git rebase main
repository /home/ada/report
rehearsal  1786178829-00

refs
  HEAD                0a7bfa18 -> 74f61021
  refs/heads/feature  0a7bfa18 -> 74f61021

graph  refs/heads/feature
  before
    * 0a7bfa1 raise the row limit
    o e353cbd add the readme
  after
    * 74f6102 (HEAD -> feature) raise the row limit
    * 355faf4 (main) add a licence
    o e353cbd add the readme

content  refs/heads/feature — every replayed commit is unchanged; the difference is the 1 commit(s) picked up from the new base
    A LICENSE

kept as 1786178829-00 — `git rehearse show 1786178829-00` to see it again
```

Note what the last block does **not** say. The tree changed — `LICENSE`
appeared — but that is the new base's own commit, which is what rebasing onto
it means. Warning about that would train you to ignore the warning.

Look at it again later, then apply it:

```console
$ git rehearse list
1786178829-00  Kept  git rebase main

$ git rehearse apply
applied:
  refs/heads/feature 74f61021291212257c19698105c43987491a29d6
  worktree reset to feature
where everything was is written down in /home/ada/report/.git/rehearse-undo
```

Applying **transplants the refs** — it moves your branches onto the exact
commits you just inspected. It never re-runs the command, so there is no second
chance for it to come out differently.

### When it conflicts

```console
$ git rehearse rebase main
CONFLICT (content): Merge conflict in config.yml
error: could not apply 9bcbf9f... raise the row limit

rehearsed  git rebase main
rehearsal  1786178811-00

the command stopped part-way on a conflict

refs
  HEAD  9bcbf9fe -> 7665595d

conflicts  stopped at 9bcbf9fe "raise the row limit"
  config.yml  1 hunk
```

Exit code 2. Your repository is untouched — no conflict markers in your
worktree, no half-finished rebase to abort. The sandbox is still there if you
want to open it and see how bad the conflict really is.

### The warning this tool exists for

A conflict you resolve by hand can silently change what a commit *does*. Here
one commit set a timeout to 90, another to 60, and the resolution said 45 —
a value neither commit ever had:

```console
$ git rehearse show 1786178840-00

warning: content drift on refs/heads/feature
  replaying a commit should not change what it does. These did change:
    changed  raise the timeout to 90
  A conflict resolution, a merge driver or a dropped commit did this.
    M config.toml
```

This is judged with `git range-diff`, commit by commit — not by diffing the two
trees. A tree diff calls every ordinary rebase "drift" and is therefore useless.

## Commands

```
git rehearse [options] rebase|merge|cherry-pick [git args...]
git rehearse [options] -- <any git command>

git rehearse list                 kept rehearsals for this repository
git rehearse show [<id>]          print a rehearsal's report again
git rehearse apply [<id>]         transplant a rehearsal into the real repo
git rehearse discard [<id>|--all] throw one, or all, away
```

`<id>` can be any unambiguous prefix. Leave it out and the most recent
rehearsal is meant.

| option | |
|---|---|
| `--apply` | apply without asking |
| `--keep` | keep without asking |
| `--todo <file>` | drive an interactive rebase from a prepared todo |
| `-h`, `--help` | usage |
| `-V`, `--version` | version |

**Our options come before the command; everything after it belongs to git.**
So `git rehearse --apply rebase -i main` is ours, and
`git rehearse rebase -i main --apply` hands `--apply` to git, which will reject
it rather than let us swallow an argument you meant for git. Use `--` to
rehearse anything else: `git rehearse -- filter-branch --tree-filter 'rm -f x'`.

## Exit codes

Stable from v0.1 on — v2's agent mode reads them.

| code | meaning |
|---|---|
| `0` | rehearsed clean, or a management command succeeded |
| `1` | an internal error in git-rehearse |
| `2` | the rehearsed command stopped part-way, usually a conflict |
| `3` | the rehearsed command failed in the sandbox |
| `4` | refused: dirty worktree, refs moved since the rehearsal, unsupported repository |

Two things worth knowing before you script this:

- **The exit code describes the rehearsal, not what became of it.** A rehearsal
  that ran cleanly and was then discarded still exits `0`.
- **With no terminal on stdin there is nobody to answer the prompt**, so the
  rehearsal is discarded and the run says so. Pass `--apply` or `--keep` to
  decide up front:

  ```bash
  git rehearse --apply rebase main || echo "did not rebase cleanly: $?"
  ```

## What it refuses

Principle 5 is *refuse loudly rather than guess*. All of these exit `4` with an
explanation rather than doing something approximate:

a dirty worktree · a bare repository · a shallow clone · a repository with
submodules · one using Git LFS · one with multiple worktrees · one with no
commits yet · and, at apply time, a repository whose refs have moved since the
rehearsal.

## Where things live

Sandboxes go in your cache directory — `~/Library/Caches/git-rehearse` on
macOS, `%LOCALAPPDATA%\git-rehearse` on Windows, `$XDG_CACHE_HOME` or
`~/.cache/git-rehearse` elsewhere. Override with `GIT_REHEARSE_CACHE_DIR`.

A kept rehearsal is pruned after seven days. A discarded one is gone
immediately. The clone hardlinks your object store rather than copying it, so
a sandbox costs almost nothing on disk, and deleting one can never touch your
real repository's objects.

Each sandbox is made **inert** at creation: remotes stripped, so an accidental
`push` inside it has nowhere to go, and `core.hooksPath` pointed at an empty
directory, so your `pre-commit` does not fire for a rehearsal.

## Why

Git has no dry run for the operations that scare people: no `git merge
--dry-run`, no rebase preview. The universal advice is "make a temp branch and
try it" — which is this tool, minus the ergonomics, the report, and the safe
apply. Simulators like [git-sim](https://github.com/initialcommit-com/git-sim)
render a *picture* of what a command would do; git-rehearse **executes the real
command** in a sandbox clone, so the answer is never an approximation, and a
result you like can be applied to your repository exactly as rehearsed.

Two ideas carry the whole design:

- **Real git executes, always.** The sandbox runs your actual `git` binary with
  your actual config. Merge drivers, rerere, attributes — everything behaves
  identically, because it is identical. Your repo-local identity, line-ending
  policy and commit signing are carried across, so a rehearsed commit is
  authored and signed exactly as the real one would have been.
- **Apply = ref transplant, never re-run.** Applying fetches the sandbox's
  objects and atomically moves your refs to the rehearsed SHAs. What you
  inspected is byte-for-byte what you get.

## Where this is going

| | |
|---|---|
| **v1** | Human CLI: `rebase` / `merge` / `cherry-pick` rehearsal, before/after graph, conflict + content-drift report, apply/discard/keep |
| **v2** | Agent mode: `--json`, stable exit codes, an MCP server — so AI coding agents rehearse history-rewriting commands *before* touching your worktree |
| **v3** | Surfaces: resolve-in-sandbox polish, `undo`, visual graph panel |

Details, non-goals and honest risks: [SCOPE.md](SCOPE.md).

## Privacy

No telemetry, no network calls, no accounts. The tool shells out to your `git`
and touches nothing beyond your repository and its own sandbox directory.
[SECURITY.md](SECURITY.md) states the full policy.

## Contributing

Issue-first — see [CONTRIBUTING.md](CONTRIBUTING.md). Quality gates are
consumed from [maxi-quality](https://github.com/maximalcode/maxi-quality)
(clippy pedantic, rustfmt, cargo-deny, Gitleaks, OSV-Scanner) and run on every
pull request.

## License

[MIT](LICENSE)
