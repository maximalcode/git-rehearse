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

> **Status: v1.2.0.** Everything on this page is in the release; every terminal
> transcript here is real captured output. The command surface and the exit
> codes are settled and will not shift under you.
> [SCOPE.md](SCOPE.md) is the full plan.

## Install

### Upgrading from v1.1.0

Kept rehearsals from v1.1.0 are not loaded by v1.2.0, which uses sandbox
metadata schema 2 to record carried work. Save any work you need from old
sandboxes before upgrading, then create fresh rehearsals. Do not edit their
metadata to force compatibility.

### A prebuilt binary

The [releases page](https://github.com/maximalcode/git-rehearse/releases)
carries an archive per platform with a SHA-256 sum beside it. Download it,
check the sum, and put the `git-rehearse` inside on your `PATH`.

| platform | archive |
|---|---|
| Linux, x86-64 | `git-rehearse-v1.2.0-x86_64-unknown-linux-gnu.tar.gz` |
| macOS, Apple silicon | `git-rehearse-v1.2.0-aarch64-apple-darwin.tar.gz` |
| Windows, x86-64 | `git-rehearse-v1.2.0-x86_64-pc-windows-msvc.zip` |

Intel macOS and ARM Linux are not built yet — [build from
source](#from-source) there, which works fine.

### From source

Needs [Rust 1.97 or newer](https://rustup.rs) and `git` on your `PATH`.

```bash
cargo install --git https://github.com/maximalcode/git-rehearse --tag v1.2.0
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
take it back with `git rehearse undo`
```

Applying **transplants the refs** — it moves your branches onto the exact
commits you just inspected. It never re-runs the command, so there is no second
chance for it to come out differently.

### Taking it back

```console
$ git rehearse undo
put back:
  refs/heads/feature 1f9ebbb0c2f4a1d90bb6dd7e7b1a58e6b4a1c0d3
  worktree reset to feature
from the apply of rehearsal 1786178829-00 at 1786178954 (unix time).
The record is used up — one apply is undoable at a time.
```

Undo is the apply run backwards, out of a record written **before** the apply
moved anything — so it survives a crash, and it works after the rehearsal
itself has been discarded or pruned. It is one transaction, and it refuses
outright unless every ref is still exactly where that apply left it: a commit
made since is work an undo would throw away, and this tool does not throw work
away to be convenient.

Three properties worth knowing before you rely on it:

- **One level deep.** There is one record per repository, so applying again
  overwrites it and a successful undo uses it up. `git rehearse undo <id>`
  refuses if the record is not the apply you meant — which is the only warning
  you can get, since a second `undo` has nothing left to work from.
- **Nothing is destroyed by an apply**, which is why this can exist at all. The
  commits it moved away from are unreferenced, not deleted, and your reflog
  keeps them for weeks. The rehearsed commits are kept too, under
  `refs/rehearse/<id>/*`, so undoing does not orphan them.
- **The record is a file you can use yourself.** Every line in
  `.git/rehearse-undo` is a complete `git update-ref` argument list — paste one
  after `git update-ref` and that ref goes back, with git refusing if it has
  moved on since.

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

### With work in progress

You do not have to commit or stash first. Uncommitted changes to tracked files
are **carried through the rehearsal**: snapshotted with `git stash create`,
which never touches your stash list, and then put back *in the sandbox* after
the command has run — so the report answers the question you actually have,
which is not "does my rebase work" but "does my rebase work **and do I get my
uncommitted work back**".

```console
$ git rehearse --apply rebase main
Rebasing (1/1)Successfully rebased and updated refs/heads/feature.

rehearsed  git rebase main
repository /home/ada/report
rehearsal  1786294984-00

refs
  HEAD                bcd0638b -> bbc67ded
  refs/heads/feature  bcd0638b -> bbc67ded

carried  1 uncommitted path(s): limits.toml
  they come back clean on the rehearsed history

applied:
  refs/heads/feature bbc67ded2e6083ff63196c4c898182fe2b1f2168
  worktree reset to feature
  1 uncommitted path(s) put back: limits.toml
```

The command itself still runs against a **clean** sandbox — `git rebase` refuses
a dirty tree, and rehearsing something git would refuse to run is not a
rehearsal. Applying then checks the tree the sandbox produced out over the reset
worktree; it does not merge your changes in your own repository for the first
time, because a first time is the failure this tool exists to prevent.

When the changes do *not* go back on cleanly, that is a stopped rehearsal like
any other — exit `2`, the sandbox kept, and the same resolve-there-and-`continue`
loop as a conflicting rebase:

```console
$ git rehearse --keep rebase main
the command ran, but your uncommitted changes did not go back on

carried  1 uncommitted path(s): config.toml
  they do NOT come back clean — 1 path(s) conflict in the sandbox:
    config.toml
```

Four things worth knowing:

- **Untracked files are left alone** — not carried, not touched. They are not in
  a stash without `-u`, and git was never going to destroy them.
- **Apply refuses if your worktree has changed since.** What the report promised
  to put back was rehearsed; anything you typed afterwards was not.
- **Everything comes back unstaged**, in your worktree, exactly where
  `git stash pop` without `--index` would leave it. A transplanted tree does not
  carry the staged/unstaged distinction.
- **`undo` refuses while those changes are in the way**, because rewinding the
  branch means `git reset --hard` and that would eat them. Stash them, undo,
  put them back.

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
git rehearse undo [<id>]          put the refs back where the last apply found them
git rehearse discard [<id>|--all] throw one, or all, away
```

`<id>` can be any unambiguous prefix. Leave it out and the most recent
rehearsal is meant. `undo` is the exception: it takes an id only to insist
which apply you mean, because there is one undo record per repository and it
always describes the most recent one.

| option | |
|---|---|
| `--apply` | apply without asking |
| `--keep` | keep without asking |
| `--json` | one JSON document on stdout instead of the report |
| `--stat-only` | the report without the before/after graphs |
| `--todo <file>` | drive an interactive rebase from a prepared todo |
| `-h`, `--help` | usage |
| `-V`, `--version` | version |

`--apply` and `--keep` work with `continue` too — it ends on the same question
a rehearsal does, so `git rehearse --keep continue <id>` is how you script a
resolve-and-carry-on loop.

`--stat-only` leaves out the before/after graphs, which are two
`git log --graph` walks per moved ref and the slowest part of printing a
report. Everything that says *what happened* stays — ref moves, carried work,
conflicts, content drift — and only the drawing of *where you were* goes. It
works on `show` and `continue` as well as on a fresh rehearsal; re-reading a
kept report is exactly when you already know the shape of the history.

**It changes what is drawn, never what is checked.** Drift detection runs
either way: it is the check that justifies this tool, the drift lines are part
of the short report, and a fast mode that switched off the safety check would
be a trap rather than a flag. With `--json` the flag does nothing at all — that
document never carried the graphs — and saying so beats refusing a harmless
combination.

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
| `4` | refused: refs or worktree moved since the rehearsal, unsupported repository |

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

## For programs: `--json`

Every command takes `--json` and answers with one document on stdout:

```console
$ git rehearse --json rebase main
{"schema":1,"id":"1786282818-00","repository":"/home/ada/report",
 "sandbox":"/home/ada/.cache/git-rehearse/report-8a2754ff/1786282818-00/sandbox",
 "command":["rebase","main"],"outcome":"stopped","exit_code":2,"conflicted":true,
 "refs":[{"name":"HEAD","before":"bff10a98…","after":"a13cff11…"}],
 "stopped_at":{"sha":"bff10a98…","subject":"raise the timeout to 90"},
 "conflicts":[{"path":"config.toml","hunks":1}],
 "drift":[],"drift_unexpected":false,"can_apply":false,"decision":"kept"}
```

Four promises, because a caller that has to guess at any of them ends up parsing
English anyway:

- **Stdout is one document and nothing else.** Git's own `Auto-merging…` and
  `CONFLICT…` go to stderr under `--json`, so the stream stays parseable.
- **Failures are documents too**, on every exit path — including a refusal
  before the command is even understood:
  `{"schema":1,"kind":"refused","message":"…","exit_code":4}`.
- **`schema` is versioned from the first release** and only changes when a field
  changes meaning or disappears. New optional fields are not a version bump.
- **It never prompts**, because there is nobody there. `--apply` and `--keep`
  decide up front; otherwise the unattended rule applies and `decision` records
  which answer was given.

So the loop a program runs is `rehearse` → read `conflicts` → resolve them under
`sandbox` → `continue` → read `drift_unexpected` → `apply`, with the real
repository untouched until that last step.

## For coding agents

The tools that stop an agent wrecking your history all work by **blocking**:
match `git rebase`, refuse, hand the problem back to you. That is the right
instinct and the wrong end of it — the agent still does not know what the
command would have done, and neither do you.

Rehearsal is the constructive version. The agent runs the real command in a
shadow clone, reads what actually happened, and only then asks for your
repository.

### Drop this in your `CLAUDE.md` or `AGENTS.md`

````markdown
## Rehearsing dangerous git commands

Before running `git rebase`, `git merge` or `git cherry-pick` in this
repository, rehearse it and act on the report:

```bash
git rehearse --json rebase main
```

Stdout is one JSON document. Read `outcome`:

- **`"clean"`** — check `drift_unexpected`. If `false`, apply it:
  `git rehearse --json apply <id>`. If `true`, **stop and show the user**
  `drift[].replay.changed`: replaying those commits changed what they do, and
  that is the thing worth catching. Do not apply it on your own judgement.
- **`"stopped"`** — a conflict. The rehearsal is kept. `conflicts[]` names the
  unmerged files and `sandbox` is the directory they are in. Resolve them
  there, `git add` them there, then `git rehearse --json continue <id>` and
  read the new document. Repeat as often as it stops.
- **`"failed"`** — git refused the command outright; `git_exit_code` is git's
  own. Nothing was kept and nothing was changed.

A document with `"kind": "refused"` means git-rehearse itself declined — read
`message` and fix what it names. Do not retry it unchanged.

The working tree is never touched until `apply`, and `apply` transplants the
commits you inspected rather than re-running anything.
````

### A PreToolUse hook

Blocks the three commands and points at the rehearsal instead. Needs `jq`, and
Claude Code's `PreToolUse` matcher set to `Bash`; exit code 2 blocks the call and
puts the message in front of the model.

```bash
#!/usr/bin/env bash
# .claude/hooks/rehearse-first.sh
command=$(jq -r '.tool_input.command // ""')

# Already going through us, or not our business.
case "$command" in
  *"git rehearse"*) exit 0 ;;
  *"git rebase"*|*"git merge"*|*"git cherry-pick"*) ;;
  *) exit 0 ;;
esac

cat >&2 <<'WHY'
Rehearse it first: `git rehearse --json <the same command>`.
Read the JSON, check drift_unexpected, then `git rehearse --json apply <id>`.
WHY
exit 2
```

### One thing this does not cover

`git reset --hard` is missing from that list on purpose. What makes it dangerous
is **uncommitted** work — the committed history it appears to destroy is
reachable through the reflog for weeks, but unstaged edits are gone for good.

git-rehearse *carries* your uncommitted work through a rehearsal and puts it
back, which is exactly why `reset --hard` does not belong here: rehearsing it
would preserve the very thing the command exists to destroy, so the rehearsal
would answer a kinder question than the one you asked. The report says so out
loud — nothing about it is silent — but an intercept that quietly turns a
destructive command into a safe one is a different product from a rehearsal. If
you want that, block it directly; do not let a rehearsal step imply a safety net
it is not providing.

## What it refuses

Principle 5 is *refuse loudly rather than guess*. All of these exit `4` with an
explanation rather than doing something approximate:

a bare repository · a shallow clone · a repository with submodules · one using
Git LFS · one with multiple worktrees · one with no commits yet · and, at apply
time, a repository whose refs have moved since the rehearsal, or whose worktree
no longer holds the uncommitted changes the rehearsal carried.

A dirty worktree used to head that list. It is now carried through the
rehearsal instead — the principle did not change, only the thing being refused.

`undo` refuses on the same principle: no record to undo, a record from a
different apply than the one you named, a ref that has moved since that apply,
a worktree with uncommitted changes it would have to rewind, and a branch that
undoing would delete while you are standing on it.

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

Repository-local `merge.*` settings are carried alongside tracked
`.gitattributes`, so custom merge drivers run as they do in the real repository.
Branch and pull workflow settings remain excluded because they describe
upstream/remotes, which the sandbox deliberately removes.

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
| **v1** | Human CLI: `rebase` / `merge` / `cherry-pick` rehearsal, before/after graph, conflict + content-drift report, uncommitted work carried through, apply/undo/discard/keep |
| **v2** | Agent mode, so AI coding agents rehearse history-rewriting commands *before* touching your worktree. `--json` and the stable exit codes are **done**; whether it also gets an MCP server is [undecided](https://github.com/maximalcode/git-rehearse/issues/37) |
| **v3** | Surfaces: resolve-in-sandbox polish, visual graph panel |

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
