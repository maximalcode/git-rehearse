# git-rehearse — Scope & Roadmap

> **Status of this document:** written 2026-08-07, before any code exists. It is the
> handoff brief for whichever session builds this. Everything marked *verified* was
> checked against live sources on that date; re-verify the load-bearing ones if months
> have passed.

## One-liner

**Rehearse dangerous git commands in a shadow clone of your real repo. See exactly what
would happen — then apply it or throw it away.**

```
git rehearse rebase -i main
# → runs the REAL rebase in a disposable clone of your repo
# → shows before/after commit graph, ref moves, conflicts, content drift
# → [a]pply to your repo · [d]iscard · [k]eep for later
```

## The end goal (read this first)

Three releases, one arc:

1. **v1.0 — the human CLI.** A terminal tool you run before any hairy rebase/merge/
   cherry-pick. Real git executes in a sandbox; you inspect the outcome; `apply` moves
   your refs to exactly the rehearsed result. ~3–5 weeks solo.
2. **v2.0 — the agent wedge (this is the actual strategic target).** JSON output, stable
   exit codes, and an MCP server so AI coding agents rehearse history-rewriting commands
   in the sandbox *before* touching the user's worktree. In 2026 agents run `git rebase`
   autonomously and people are visibly afraid of that; no tool occupies this. The CLI in
   v1 exists partly to prove the mechanics and partly because the author will use it —
   but the growth story is agents.
3. **v3.x — surfaces.** Conflict-resolution flow polish, `undo`, dirty-worktree
   snapshots, and (optionally) a visual before/after graph panel inside
   [git-city](https://github.com/maximalcode/git-city), which already renders repos and
   ships a git client.

The product is **not** a git teaching game. Research found zero demand evidence for a
drill/lesson layer, and [learnGitBranching](https://github.com/pcottle/learnGitBranching)
(33k★) owns teaching. Don't build it. See Non-goals.

## Why this exists — the verified gap

Git has no dry run for the operations that scare people. There is no
`git merge --dry-run`, no rebase preview. The universal Stack Overflow advice is "make a
temp branch and try it" — which is exactly this tool, minus the ergonomics, the report,
and the safe apply.

The competitive landscape (verified 2026-08-07):

| Project | Stars | What it does | What it doesn't |
|---|---|---|---|
| [git-sim](https://github.com/initialcommit-com/git-sim) | ~4.7k | Simulates 23 commands against your repo, renders **image/video only** | Never executes, never sandboxes, has **no apply mode** — confirmed from its own docs. Re-checked 2026-08-09: unchanged, and dormant (below) |
| [learnGitBranching](https://github.com/pcottle/learnGitBranching) | ~33.8k | Teaching sandbox with a **fake** git and fixed lessons | Not your repo, not real git |
| [git-branchless](https://github.com/arxanas/git-branchless) | ~4.1k | Best-in-class `git undo` | Repairs after the fact; no rehearsal surface |
| [jj](https://github.com/jj-vcs/jj) | ~30.9k | Operation log makes every op undoable | **Structural substitute — the #1 risk.** Mitigation: installing a CLI is a far smaller ask than switching VCS |
| destructive_command_guard | ~5.7k in 7 months | Blocks agents from running dangerous git commands | Blocks; doesn't let you *find out* what would happen |

The surviving angle, precisely: **execution + inspection + apply-or-discard**, and
secondarily the agent-facing version of that. Nobody occupies either half.

**Name check (2026-08-07, registries settled 2026-08-09):** `git-rehearse` is free on
GitHub search, **free on crates.io** and **free in Homebrew core**; nearest neighbor is
`openshift/pj-rehearse` (Kubernetes prow CI, unrelated). One adjacent name is gone: the
bare `rehearse` crate was taken 2026-06-22 — effect-aware operation planning, not git,
57 downloads — which costs us nothing, since the binary must be called `git-rehearse` for
git to pick it up as a subcommand anyway.

## Design principles (locked — a future session should not relitigate these)

1. **Real git executes, always.** We never reimplement or approximate git semantics.
   The sandbox runs the user's actual `git` binary with the user's actual config
   (merge drivers, rerere, attributes all behave identically). git-sim's weakness is
   simulation fidelity; ours is definitionally perfect.
2. **Apply = ref transplant, never re-run.** After a successful rehearsal, applying must
   produce byte-for-byte the rehearsed result: fetch the sandbox's objects into the real
   repo and atomically update refs to the rehearsed SHAs. Re-running the command in the
   real repo could diverge (timestamps, interactive input, hooks) and would discard any
   conflict resolution done in the sandbox. This is the core correctness invariant.
3. **The sandbox is disposable and inert.** No remotes (stripped at creation — an
   accidental `push` inside the sandbox must have nowhere to go), hooks disabled by
   default (`--with-hooks` to opt in), lives under the user cache dir, auto-pruned.
4. **Zero telemetry, zero network, zero spend.** Matches every other maximalcode repo.
   The tool itself never phones anywhere; CI is GitHub Actions free tier.
5. **Refuse loudly rather than guess.** Dirty worktree in v1.0 → refuse with a clear
   message (git itself refuses rebase on a dirty tree, so this matches expectations).
   Refs moved between rehearse and apply → refuse (race detection, see below).

## Mechanics — how a rehearsal works

```
┌─ real repo ──────────┐      ┌─ sandbox (cache dir) ─────────────┐
│ 1. snapshot refs+HEAD │ ──▶ │ 2. git clone --local (hardlinks)  │
│    (pre-state file)   │      │ 3. strip remotes, disable hooks  │
│                       │      │ 4. run the REAL command           │
│ 7. apply: fetch objs, │ ◀── │ 5. analyze: DAG diff, ref moves,  │
│    verify pre-state   │      │    conflicts, content drift       │
│    unchanged, atomic  │      │ 6. report to terminal             │
│    update-ref txn     │      └───────────────────────────────────┘
└───────────────────────┘
```

Step details a builder needs:

- **2 — clone:** `git clone --local --no-checkout <real> <sandbox>` then checkout the
  relevant branch. `--local` hardlinks objects → fast and cheap even on big repos
  (Windows falls back to copy; acceptable). Deliberately **not** `--shared`/alternates:
  the sandbox must survive a concurrent `git gc` in the real repo. Deliberately **not**
  `git worktree`: worktrees share refs with the main repo, which is precisely the
  mutation we're sandboxing away.
- **4 — execute:** spawn the user's `git` with env passthrough. Interactive commands
  (`rebase -i`) open the user's real editor against the sandbox — that's fine and
  desirable. Also accept `--todo <file>` to inject a rebase todo non-interactively
  (sets `GIT_SEQUENCE_EDITOR`); cheap now, load-bearing for v2 agents.
- **5 — analysis:**
  - *Ref moves:* every `refs/heads/*` + `HEAD` whose SHA changed, old → new.
  - *DAG diff:* render the affected subgraph (merge-base .. tips) before and after,
    ASCII, side by side or stacked. Do not render the whole history.
  - *Conflicts:* if the command stopped on conflicts, list files with conflict-hunk
    counts, and which commit was being replayed when it stopped.
  - *Content drift:* for rebase/cherry-pick, diff the old tip tree vs the new tip tree.
    Expected empty; non-empty (without conflict resolution) is the classic silent-
    semantic-change signal and gets a loud warning. This check alone justifies the tool.
- **7 — apply:** `git fetch <sandbox-path> +refs/heads/*:refs/rehearse/<id>/*`, verify
  every ref in the pre-state file still matches the real repo (abort if anything moved —
  someone committed meanwhile), then a single `git update-ref --stdin` transaction. If
  the checked-out branch was rewritten: verify worktree is still clean, then
  `git reset --hard` to the new tip. Write the pre-state SHAs to
  `.git/rehearse-undo` and print them (manual recovery in v1; `undo` command in v1.x).

## v1.0 — exact scope

**Command surface:**

```bash
git rehearse rebase [args...]        # incl. -i, --onto, --todo <file>
git rehearse merge [args...]
git rehearse cherry-pick [args...]
git rehearse -- <any git command>    # generic escape hatch: ref-moves + drift report only
git rehearse list                    # kept rehearsals
git rehearse show [<id>]             # re-print a report
git rehearse apply [<id>]            # apply a kept rehearsal
git rehearse discard [<id>|--all]
```

Installed as a `git-rehearse` binary on PATH → `git rehearse` works automatically as a
git subcommand. Default flow after a rehearsal: print report, then prompt
`[a]pply / [d]iscard / [k]eep`. Flags `--apply` / `--keep` for scripting; non-TTY default
is discard-unless-`--apply`. (An earlier draft listed a `--json=off` flag here, which
contradicted the out-list below: v1.0 has no JSON output in any spelling, so there is
nothing to switch off. JSON arrives opt-in with `--json` in v2.)

**Exit codes (stable from day one — v2 depends on them):**
`0` rehearsed clean · `2` stopped on conflicts · `3` command failed in sandbox ·
`4` refused (dirty tree, ref race, not a repo) · `1` internal error.

**Storage:** `$XDG_CACHE_HOME/git-rehearse/<repo-id>/<rehearsal-id>/` containing the
sandbox + `meta.json` (command, pre-state refs, result, timestamps). Discarded sandboxes
deleted immediately; kept ones pruned after 7 days with a warning in `list`.

**Stack:** Rust. [gix (gitoxide)](https://github.com/GitoxideLabs/gitoxide) for
read-only analysis (ref enumeration, DAG walk, tree diff); shell out to real `git` for
everything that mutates (clone, the rehearsed command, fetch, update-ref). If gix API
friction burns more than a day on any one feature, shell out for that too — gix is an
optimization, not a principle. Distribution: `cargo install`, prebuilt binaries for
macOS/Linux/Windows via GitHub Releases (cargo-dist), Homebrew tap later.

**Platform matrix in CI:** ubuntu / macos / windows, stable Rust. Integration tests are
scripted git repos (fixtures built by test code, not checked-in `.git` dirs) covering:
clean rebase, conflicting rebase, `--onto`, octopus merge, cherry-pick range, rewritten
checked-out branch, ref race on apply, dirty-tree refusal, evil-merge drift detection.

**Explicitly OUT of v1.0** (someone will be tempted; resist):
- Dirty-worktree snapshotting (v1.x)
- Conflict resolution *inside* the sandbox + resume (v1.x — the resume half,
  `git rehearse continue`, shipped after v0.1.0; see v1.x item 3)
- `undo` command (v1.x — pre-state file + printed SHAs cover recovery meanwhile)
- JSON output, MCP, any agent affordance beyond `--todo` and exit codes (v2)
- Any GUI (v3)
- `push`/`pull` rehearsal — push mutates remote state and cannot be truly rehearsed;
  `pull` decomposes into fetch (already safe) + merge/rebase (already covered)

**v1.0 done means:** the author reaches for it unprompted before real operations in
hollowreach / maxi-editor / git-city, and it has applied at least one multi-commit
interactive rebase to a real repo with zero surprises.

## v1.x — quality-of-life (order by annoyance, ship as patch releases)

1. `git rehearse undo` — restore pre-state refs from the undo file, same race checks.
2. Dirty worktree: snapshot via `git stash create` (no stash-list pollution), replicate
   into sandbox, unstash on apply.
3. Resolve-in-sandbox. **`git rehearse continue` is built** (post-v0.1.0, issue #38): the
   conflict report prints the sandbox path and the exact command, you resolve there with
   whatever tools you already use, and `continue` runs the matching `--continue`,
   re-analyzes and offers apply. It sits in this list for history only — **it belongs to
   v2, not to quality-of-life**, because the MCP conflict flow cannot exist without it.
   Still unbuilt and now optional: the `[r]esolve` prompt that drops into `$SHELL` inside
   the sandbox, which printing the path makes largely redundant. (Resolutions carry over
   on apply automatically — principle 2 gives us this for free.)
4. `--stat-only` fast mode; report paging; color config.

## v2.0 — agent mode (the strategic release)

Everything here rides on v1's mechanics; nothing requires rework if v1 keeps its exit
codes and `--todo` stable.

- **`--json`:** one machine-readable report document — schema versioned from day one
  (`"schema": 1`), containing command, exit class, ref moves, conflict list (file +
  hunk counts + stopped-at commit), drift diffstat, and sandbox id. **There is no
  separate "apply token"**; an earlier draft listed one and never said what it was. The
  sandbox id already names a rehearsal, and the refs-moved race check reads the
  pre-state out of `meta.json` itself. Handing the caller a fingerprint to pass back to
  `apply` would move that check out of the tool and into the agent, which is the
  opposite of principle 5.
- **MCP server:** `git-rehearse mcp` (stdio). Tools: `rehearse(command, repo)`,
  `inspect(id)`, `resolve_file(id, path, content)`, `continue(id)`, `apply(id)`,
  `discard(id)` — mirroring the CLI, `continue` included, since a stopped rehearsal is
  the case an agent hits most. An agent's rebase story becomes: rehearse → read
  conflicts → write resolutions into the sandbox → `continue` → re-inspect → apply. The
  user's worktree is untouched until `apply`.
- **Docs as product:** a copy-paste CLAUDE.md/AGENTS.md snippet ("before any
  history-rewriting git command, rehearse it and act on the report") and a PreToolUse
  hook example that intercepts `git rebase|merge|reset --hard` in Bash calls and reroutes
  through rehearse. The README section for this *is* the marketing.
- **Launch narrative:** "your agent can rehearse a rebase before it touches your repo" —
  aimed at the audience that gave destructive_command_guard 5.7k stars in seven months
  for *blocking* these commands. Rehearsal is the constructive version of blocking.

## v3.x — surfaces (sketch only; decide when v2 has users)

- git-city integration: a rehearsal panel showing before/after DAG (or city diff) —
  git-city already ships the renderer and the git client.
- `git rehearse` TUI mode (ratatui) with navigable graph.
- Rehearsal sequences: queue several commands, rehearse the composite.

## Non-goals — permanent

- **No teaching game / drills / lessons.** Zero demand evidence; learnGitBranching owns it.
- **No git reimplementation.** The day real-git-execution is compromised for speed is the
  day the tool's one guarantee dies.
- **No telemetry, no accounts, no server.**
- **No push rehearsal** (see v1 out-list for why).

## Risks & kill criteria

| Risk | Read |
|---|---|
| **jj** makes rehearsal obsolete as it grows | Real, structural. Bet: git's installed base moves slower than jj grows, and agents operate in git repos today. **Still open (#40), deliberately not re-checked** — the bet is from 2026-08-07 and a trend question has a minimum sampling interval; re-stamping it two days later would only manufacture confidence. Trigger: the #36 checkpoint (2026-09-09) or the decision to start building v2, whichever is first. The question then is *not* jj's star count but whether the **agent** audience is moving to jj — v2's premise is that agents run `git rebase` in git repos. |
| git-sim adds an execute/apply mode | **Closed out 2026-08-09 (#40).** Its releases were the thing to check, and there are none to check: `v0.3.5` is the latest on both GitHub and PyPI, dated **2024-04-16**, and the only commits since are two README touch-ups. Stars flat at 4,672. A dormant Manim renderer does not grow an execute/apply mode; re-open this row only if it ships again. |
| Usage frequency is low for humans (a few times/month) | True — this is *why* v2 exists. Agents rehearse constantly; humans occasionally. |
| Apply-mechanism edge cases (submodules, LFS, worktrees, shallow clones) | Refuse loudly on all four in v1.0 (detect and exit 4 with an explanation). Support only if users actually ask. |

**Kill criterion** (steal from the research report): if after v1.0 the author does not
reach for it unprompted within a month of normal work across nine repos, the human CLI
is a museum piece — either pivot everything to v2/MCP immediately or stop.

## Repo setup checklist (maximalcode house standard)

**Quality baseline comes from [maxi-quality](https://github.com/maximalcode/maxi-quality),
not hand-rolled here.** Prerequisite: maxi-quality needs a Rust Layer 1 first (issue +
PR there before this repo's first commit): `configs/rust/` with `rustfmt.toml`, a
`deny.toml` template, and a `[workspace.lints]` template (clippy pedantic + selected
nursery; Cargo cannot consume lint config remotely, so adopt.sh writes it — same pattern
as the C# props). Extend adopt.sh detection with `Cargo.toml`. Layer 2 (Semgrep generic +
Gitleaks + OSV-Scanner, which reads Cargo.lock natively) needs **zero changes** — adopt
it on day one. Note: Semgrep's Rust language support is experimental; clippy is the
conventions layer for Rust.

- [x] maxi-quality Rust Layer 1 (its PR #59) adopted via `adopt.sh --hooks` in the first
      commit — pre-commit gitleaks (staged-index scan) + Layer 2 CI gate
- [x] `unsafe_code = "forbid"` — enforced at the manifest level by the adopted `[lints]`
      block (stronger than the crate-root attribute: `forbid` can't be waived per-file),
      plus the attribute in `main.rs`
- [x] MIT license; `README.md` with the one-liner (v0.1.0 shipped without the
      terminal-cast GIF — the README carries real captured transcripts instead; whether
      the GIF is still wanted is #31)
- [x] `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`
- [x] CI: `cargo fmt --check` + clippy `-Dwarnings` + `cargo deny check advisories bans`
      + tests, pinned toolchain, plus maxi-quality Layer 2 (all scaffolded by adopt.sh;
      ubuntu-only for now — the os matrix comes with v0.1 code that has platform risk)
- [x] Issue-first workflow, `develop` → `main`, branch naming `<type>/<issue>-<slug>`
      (`CLAUDE.md` §3; branch protection set at repo creation)
- [x] `CLAUDE.md`: build/test commands, the five design principles verbatim, and the
      rule that principle 2 (apply = ref transplant) is inviolable
- [x] Releases: tag `v*` on main → `.github/workflows/release.yml` builds
      linux/macos/windows and attaches them with checksums. Hand-written rather than
      cargo-dist (#10): v0.1 needs three binaries on a release page, not installers,
      a checksum manifest and a Homebrew tap. Revisit when it does.
- [x] Name collision check: crates.io verified free for `git-rehearse` and
      `git_rehearse` (2026-08-07); claimed with the first real `cargo publish`, not a
      placeholder

## Open questions for the building session (small; don't block on them)

1. Rebase `--todo` injection: `GIT_SEQUENCE_EDITOR` script vs. writing
   `.git/rebase-merge/git-rebase-todo` directly — prototype the env-var route first.
2. ASCII DAG rendering: hand-roll (bounded subgraph makes it tractable) vs. delegate to
   `git log --graph` on before/after states. Delegating is an acceptable v1 shortcut.
3. `repo-id` for the cache dir: hash of canonicalized repo path is enough.
4. Whether `git rehearse -- git commit ...` (non-history-rewriting commands) should warn
   that rehearsal is pointless, or just work. Just working is probably fine.
