#!/usr/bin/env python3
"""`PreToolUse` hook on `Bash`: refuse a commit or push that skips the hooks.

WHAT IT ENFORCES

`adopt.sh --hooks` installs `hooks/pre-commit`, and that hook is the last
thing between a working tree and a commit the gate never saw. It is also
switched off by seven characters. `git commit --no-verify` is not a hostile
act — it is what a session reaches for when a commit is refused and the
refusal looks like plumbing — and it turns the local half of this baseline
off silently. So: the flag is refused, on `commit` and on `push`, and the
refusal says which gate it is protecting.

WHY A HOOK AND NOT A `permissions.deny` RULE

The other half of this contract IS a deny rule, so the asymmetry is worth
stating. Two facts from the permissions reference (read 2026-08-23) rule it
out here:

  * `Bash(command:...)` is not a usable rule. The docs: a rule like
    `Bash(command:rm *)` "would be bypassable by a compound command, so
    Claude Code ignores it and emits a startup warning." A rule that is
    accepted, never consulted, and warns once at startup is indistinguishable
    from protection.
  * `Bash(git commit --no-verify *)` is the prefix form, and the docs devote a
    warning to how fragile argument-matching is: options before the argument,
    variables, extra spaces. Every bypass in that list applies here.

Their recommended answer to both is this file: "use PreToolUse hooks". So the
tokenizer below is the thing under test, and `samples/agent-guard/` tests it
as one — including the two shapes a substring search gets backwards, a flag
inside a commit MESSAGE and a flag on the far side of an `&&`.

WHAT IT DELIBERATELY DOES NOT CLAIM

It reads the command Claude asked to run. It does not read what that command
then does: a shell script, a `Makefile` target or an alias that itself commits
with `--no-verify` passes this hook, because the flag is not in the text. That
is the same boundary `sample-guard.py` has and it is the same answer — this
guards drift, not malice. `hooks/pre-commit` is not a security control and
this is not one either.

AND IT FAILS OPEN ON ITS OWN PLUMBING

A command the tokenizer cannot read — an unbalanced quote — allows and warns.
An agent hook is not bypassable by the party it constrains, so a parser that
refuses what it cannot parse turns every odd command into a wall.
"""

from __future__ import annotations

import os
import shlex
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from guard import ALLOW, deny_tool, read_event, warn  # noqa: E402

# Options that swallow the FOLLOWING word. This list is why the guard is a
# tokenizer: `git commit -m --no-verify` commits with an unfortunate message
# and skips nothing, and `-F --no-verify` reads a strangely-named file. A
# guard that denies either is wrong in the direction that gets it deleted.
#
# `-S`/`--gpg-sign` and `--cleanup` are deliberately ABSENT: their argument is
# optional and attached (`-Skeyid`, `--cleanup=scissors`), so consuming the
# next word would eat the very flag we are looking for in `git commit -S -n`.
GLOBAL_VALUE_OPTS = frozenset({
    "-c", "-C", "--git-dir", "--work-tree", "--namespace", "--exec-path",
    "--config-env",
})

# Per subcommand: the long flag that skips the hooks, the short letter that
# means the same thing, and the options that take a value.
#
# The short letter is the asymmetry that matters and it is git's, not ours:
# `-n` is `--no-verify` for `commit` and `--dry-run` for `push`. Denying
# `git push -n` would block the safest command in git.
SUBCOMMANDS = {
    "commit": {
        # `hooks/pre-commit` is the hook `adopt.sh --hooks` installs, and it is
        # the only one this baseline ships. Naming it is the point: a refusal
        # that does not say what it is protecting is a refusal the session
        # argues with.
        "skips": ("`hooks/pre-commit` — the local half of this repo's quality "
                  "gate, and the last check before content the gate has not "
                  "seen becomes a commit."),
        "short": "n",
        "value_long": frozenset({"-m", "--message", "-F", "--file", "-C",
                                 "--reuse-message", "-c", "--reedit-message",
                                 "--author", "--date", "-t", "--template",
                                 "--fixup", "--squash", "--trailer",
                                 "--pathspec-from-file"}),
        "value_short": "mFCct",
    },
    "push": {
        # NOT pre-commit. `--no-verify` on a push skips `pre-push`, which this
        # baseline does not ship — so what it switches off is whatever the repo
        # or the developer installed there. Saying "pre-commit" here would be
        # the message claiming to protect something it does not.
        "skips": ("the `pre-push` hook. This baseline ships no `pre-push` of "
                  "its own, so what you are skipping is whatever this repo or "
                  "this machine installed there — and a push is the last point "
                  "before the content reaches everyone else."),
        "short": None,
        "value_long": frozenset({"--repo", "-o", "--push-option",
                                 "--receive-pack", "--exec"}),
        "value_short": "o",
    },
}

SKIP_FLAG = "--no-verify"

# Anything made only of these is a shell operator, not a word. `shlex` in
# `punctuation_chars` mode emits them as their own tokens, which is the whole
# reason for using it over `shlex.split`: `a&&b` is two commands, and to
# `shlex.split` it is one word.
#
# It splits `(` and `)` too, so `$(git commit --no-verify)` is read as a
# command and denied. That is stricter than a shell — the substitution's output
# would be the argument, not a commit — and it is left that way deliberately:
# the over-strict direction here refuses a command nobody writes, and the
# permissive direction is a bypass.
OPERATOR_CHARS = set(";&|()<>")


def simple_commands(command: str) -> list[list[str]] | None:
    """Every simple command in the string, tokenised. None if unreadable.

    Lines are split BEFORE lexing. `shlex` treats a newline as ordinary
    whitespace, so a two-line script lexes into one argv and the second
    command's own name is read as an argument to the first.
    """
    out: list[list[str]] = []
    for line in command.splitlines():
        lexer = shlex.shlex(line, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        try:
            tokens = list(lexer)
        except ValueError:
            return None
        current: list[str] = []
        for token in tokens:
            if token and set(token) <= OPERATOR_CHARS:
                if current:
                    out.append(current)
                current = []
                continue
            current.append(token)
        if current:
            out.append(current)
    return out


def git_invocation(argv: list[str]) -> tuple[str, list[str]] | None:
    """(subcommand, its arguments) if this simple command is `git <sub>`.

    Leading `VAR=value` assignments are stepped over because a shell allows
    them in front of any command, and the basename is compared because
    `/usr/bin/git` is the same program as `git`.
    """
    i = 0
    while i < len(argv) and "=" in argv[i] and not argv[i].startswith("-"):
        name = argv[i].split("=", 1)[0]
        if not name or not name.replace("_", "a").isalnum():
            break
        i += 1
    if i >= len(argv):
        return None
    if os.path.basename(argv[i]) not in ("git", "git.exe"):
        return None
    i += 1

    # Global options sit BETWEEN `git` and the subcommand, and one of them
    # takes a value: `git -c core.hooksPath=/dev/null commit --no-verify`.
    # Read argv[1] as the subcommand and this bypass is free.
    while i < len(argv) and argv[i].startswith("-"):
        token = argv[i]
        i += 1
        if token in GLOBAL_VALUE_OPTS:
            i += 1
    if i >= len(argv):
        return None
    return argv[i], argv[i + 1:]


def offending_flag(sub: str, args: list[str]) -> str | None:
    """The hook-skipping flag in these arguments, or None."""
    spec = SUBCOMMANDS[sub]
    i = 0
    while i < len(args):
        token = args[i]
        i += 1
        # Everything after `--` is a pathspec. `git commit -m x -- --no-verify`
        # stages a badly-named file and skips nothing.
        if token == "--":
            return None
        if token.startswith("--"):
            # The `=` split is what stops `--message=x --no-verify` from
            # reading as "the message option, which eats the next word".
            # Comparing `name` rather than `token` on the line below is the
            # same split doing nothing: `--no-verify` takes no value, so no
            # fixture can tell the two spellings apart. Said here rather than
            # left looking covered.
            name, attached, _value = token.partition("=")
            if name == SKIP_FLAG:
                return SKIP_FLAG
            if not attached and name in spec["value_long"]:
                i += 1
            continue
        if token.startswith("-") and len(token) > 1:
            # Short flags bundle: `-an` is `-a -n`. Comparing the whole token
            # to `-n` misses every spelling but the bare one.
            for pos, ch in enumerate(token[1:], start=1):
                if spec["value_short"] and ch in spec["value_short"]:
                    # The rest of the cluster is this option's value, or the
                    # next word if the cluster ends here.
                    if pos == len(token) - 1:
                        i += 1
                    break
                if spec["short"] and ch == spec["short"]:
                    return "-" + ch
            continue
        # A positional. `git push origin HEAD` keeps going; a subcommand's
        # arguments are not ordered.
    return None


def main() -> int:
    event = read_event()
    if event is None:
        warn("stdin was not a JSON object; allowing")
        return ALLOW

    ti = event.get("tool_input")
    if event.get("tool_name") != "Bash" or not isinstance(ti, dict):
        return ALLOW
    command = ti.get("command")
    if not isinstance(command, str) or not command.strip():
        return ALLOW

    parsed = simple_commands(command)
    if parsed is None:
        warn("the command could not be tokenised; allowing")
        return ALLOW

    for argv in parsed:
        found = git_invocation(argv)
        if found is None:
            continue
        sub, args = found
        if sub not in SUBCOMMANDS:
            continue
        flag = offending_flag(sub, args)
        if flag is None:
            continue
        deny_tool(
            f"This `git {sub}` passes {flag}, which switches off "
            f"{SUBCOMMANDS[sub]['skips']}\n\nRun it without the flag. If the "
            "hook is failing for a reason that is not your change — a broken "
            "toolchain, a pre-existing failure — say so and a human will "
            "decide; skipping it silently is how that failure becomes "
            "everyone's."
        )
        return ALLOW

    return ALLOW


if __name__ == "__main__":
    sys.exit(main())
