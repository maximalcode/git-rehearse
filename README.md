# git-rehearse

**Rehearse dangerous git commands in a shadow clone of your real repo. See
exactly what would happen — then apply it or throw it away.**

```
git rehearse rebase -i main
# → runs the REAL rebase in a disposable clone of your repository
# → shows before/after commit graph, ref moves, conflicts, content drift
# → [a]pply to your repo · [d]iscard · [k]eep for later
```

> **Status: pre-v0.1.** There is nothing to install yet. [SCOPE.md](SCOPE.md)
> is the full plan — v1 scope, the roadmap through the agent-facing v2, and the
> design principles the implementation is held to. Watch releases if you want
> to know when the first usable build lands.

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
  identically, because it is identical.
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
