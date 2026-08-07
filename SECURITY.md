# Security

git-rehearse is a CLI that clones your repository into a local sandbox, runs
git commands there, and — only when you say so — moves your refs. That is a
lot of trust, so this document states plainly what it does with your data,
what it runs, and how to report a problem. The policy is written down now,
before v0.1 exists, so the code is held to it rather than the other way
around.

## Reporting a vulnerability

Use GitHub's private reporting:
**[Report a vulnerability](https://github.com/maximalcode/git-rehearse/security/advisories/new)**.
That opens a private advisory only maintainers can read.

Please **do not** open a public issue for anything exploitable.

Include what you did, what happened, your OS and git-rehearse version, and a
proof of concept if you have one. This is a solo hobby project, so expect a
first reply within about a week rather than within hours. There is no bug
bounty.

Supported: the **latest release**. Fixes go into the next release rather than
being backported. (Pre-v0.1: there is no release yet; report against `main`.)

## What leaves your machine

**Nothing.** git-rehearse makes no network requests of its own — no telemetry,
no analytics, no crash reporting, no update checks, no accounts. The sandbox
clone is created with its remotes stripped, so even git inside the sandbox has
nowhere to push or fetch.

## Credentials

git-rehearse **stores no tokens, passwords or keys** and never prompts for
one. Everything credential-shaped stays with git and your own credential
helper, in your real repository, where rehearsals never reach.

## What it executes

Your own `git` binary, found on `PATH` — it bundles none. Repository hooks are
**disabled inside the sandbox by default** (`--with-hooks` opts in), because a
hook is code from the repository being rehearsed, and rehearsing a repo should
not be the thing that runs it.
