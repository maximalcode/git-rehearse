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

> **Status: v0.1.0 — the first release.** Everything on this page works; it is
> what produced every terminal transcript here. The v1 command surface and the
> exit codes are settled and will not shift under you.
> [SCOPE.md](SCOPE.md) is the full plan, including the agent-facing v2 that is
> the real target.

## Install

### A prebuilt binary

The [releases page](https://github.com/maximalcode/git-rehearse/releases)
carries an archive per platform with a SHA-256 sum beside it. Download it,
check the sum, and put the `git-rehearse` inside on your `PATH`.

| platform | archive |
|---|---|
| Linux, x86-64 | `git-rehearse-v0.1.0-x86_64-unknown-linux-gnu.tar.gz` |
| macOS, Apple silicon | `git-rehearse-v0.1.0-aarch64-apple-darwin.tar.gz` |
| Windows, x86-64 | `git-rehearse-v0.1.0-x86_64-pc-windows-msvc.zip` |

Intel macOS and ARM Linux are not built yet — [build from
source](#from-source) there, which works fine.

### From source

Needs [Rust 1.97 or newer](https://rustup.rs) and `git` on your `PATH`.

```bash
cargo install --git https://github.com/maximalcode/git-rehearse --tag v0.1.0
```

Or from a clone, to track `develop`:

```bash
git clone https://github.com/maximalcode/git-rehearse && cd git-rehearse && cargo install --path .
```

Not on crates.io yet.

However you install it, you get a binary called `git-rehearse`. Because git
treats any `git-<name>` on your `PATH` as a subcommand, that is all it takes
for `git rehearse …` to work — no alias, no config.

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
$ git rehearse --keep rebase main
CONFLICT (content): Merge conflict in config.toml
error: could not apply 1f9ebbb... raise the timeout to 90

rehearsed  git rebase main
rehearsal  1786276260-00

the command stopped part-way on a conflict

refs
  HEAD  1f9ebbbc -> 350ba0a4

conflicts  stopped at 1f9ebbbc "raise the timeout to 90"
  config.toml  1 hunk

to work on it:
  cd ~/Library/Caches/git-rehearse/app-b0db48dc0bcbea3e/1786276260-00/sandbox
  # resolve the conflict, then `git add` the files
  git rehearse continue 1786276260-00
```

Exit code 2. Your repository is untouched — no conflict markers in your
worktree, no half-finished rebase to abort. The conflict is a real one, in a
real repository, and it is waiting for you somewhere harmless.

Resolve it there however you like, then carry on:

```console
$ git rehearse continue 1786276260-00
```

`continue` runs `git rebase --continue` in the sandbox, re-reads what happened
and prints the report again. Repeat as often as the rebase stops. It refuses,
rather than letting git complain, if anything is still unmerged:

```console
$ git rehearse continue
git-rehearse: 1 path(s) are still unmerged in the sandbox:
  config.toml
Resolve them and `git add` them there, then continue.
```

Because applying transplants the sandbox's commits rather than re-running the
command, **the resolution you did in the sandbox is the resolution you get.**
You never resolve the same conflict twice.

### The warning this tool exists for

A conflict you resolve by hand can silently change what a commit *does*. Above,
one commit set the timeout to 90 and the other to 60. Resolve it to 45 — a
value neither commit ever had — and `continue` says so:

```console
$ git rehearse continue 1786276260-00

warning: content drift on refs/heads/feature
  replaying a commit should not change what it does. These did change:
    changed  raise the timeout to 90
  A conflict resolution, a merge driver or a dropped commit did this.
    M config.toml
```

That is not a complaint about the conflict — it is the report noticing that the
commit no longer does what it used to. Sometimes that is exactly what you meant.
The point is that you find out before it is in your repository, not after.

This is judged with `git range-diff`, commit by commit — not by diffing the two
trees. A tree diff calls every ordinary rebase "drift" and is therefore useless.

## Commands

```
git rehearse [options] rebase|merge|cherry-pick [git args...]
git rehearse [options] -- <any git command>

git rehearse list                 kept rehearsals for this repository
git rehearse show [<id>]          print a rehearsal's report again
git rehearse continue [<id>]      carry on a stopped one, once it is resolved
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

`--apply` and `--keep` work with `continue` too — it ends on the same question
a rehearsal does, so `git rehearse --keep continue <id>` is how you script a
resolve-and-carry-on loop.

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
  answer is given for you and the run says which one it gave. A rehearsal that
  ran cleanly is **discarded** — you can always run the command again. One that
  **stopped part-way is kept**, because its sandbox is the only copy of where it
  got to, and discarding it would delete the path and the `continue` command the
  report just printed. Nothing is ever applied unasked. Pass `--apply` or
  `--keep` to decide up front:

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
