#!/usr/bin/env python3
"""Run the gate, and record what it verified.

    python3 scripts/agent-guard/record-gate.py --gate
    python3 scripts/agent-guard/record-gate.py -- npm run gate

Runs the command with its output going where it always would, then writes
`.claude/agent-guard-receipt.json` describing the verdict and a fingerprint of
exactly the content the command saw. The command's own exit code is this
script's exit code, so putting the wrapper in front of a gate never changes
what CI or a human sees.

THE TWO FORMS, AND WHY THERE ARE TWO (#178)

`--` takes an argv the caller's own shell has already split. `--gate` takes no
command at all: it reads `gate_command` out of `.claude/agent-guard.json` and
runs THAT, whole, through one shell.

The second form exists because the first cannot be printed. The Stop hook's
refusal has to tell a session what to run, and interpolating a declared gate
into `record-gate.py -- <gate>` put any `&&`, `;` or `|` in it OUTSIDE this
wrapper: pasted, half the gate ran unrecorded and the receipt said "pass" for
a run whose second half had failed. `--gate` carries no operators, so there is
nothing left to paste wrong.

Note which input each form takes, because it is the whole of the reasoning
below about shells: `--` receives an argv, and `--gate` receives a string that
was never anything but a shell command.

WHY THE FINGERPRINT IS TAKEN BEFORE, NOT AFTER

A formatter is a gate that edits. Fingerprinting afterwards would record the
post-format tree as verified when what was checked was the pre-format one —
and worse, it would record a passing verdict for content no tool has read.
Taken before, a gate that rewrites files leaves a receipt that no longer
matches, and the next stop asks for another run. That is the correct outcome
and it is cheap: the second run is a no-op on already-formatted code.

WHY A FAILING RUN IS RECORDED AT ALL

Deleting the receipt on failure and writing one on success would be simpler,
and it would make "the gate failed" indistinguishable from "the gate never
ran". They deserve different messages: one is a bug to fix, the other is a
step to take. So the verdict is a field, not the file's existence.
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from guard import (  # noqa: E402
    CONFIG,
    RECEIPT,
    fingerprint,
    gate_argv,
    gate_command,
    is_declared_gate,
    repo_root,
)

USAGE = ("usage: record-gate.py --gate\n"
         "       record-gate.py [--] <command> [args...]")


def main(argv: list[str]) -> int:
    args = argv[1:]
    declared = None
    want_gate = bool(args) and args[0] == "--gate"
    if args and args[0] == "--":
        args = args[1:]
    if not args:
        print(USAGE, file=sys.stderr)
        return 3

    root = repo_root()
    if root is None:
        print("record-gate: not inside a git working tree", file=sys.stderr)
        return 3

    if want_gate:
        if len(args) > 1:
            print("record-gate: --gate takes no command; it runs the one "
                  f"{CONFIG} declares", file=sys.stderr)
            return 3
        declared = gate_command(root)
        if declared is None:
            # Nothing is written. A receipt for a gate that was never named
            # would be a pass for a run that did not happen, which is worse
            # than the absent receipt the Stop hook already knows how to
            # report.
            print(f"record-gate: no gate_command in {CONFIG}. Declare it "
                  'once:\n\n  echo \'{ "gate_command": "<your gate>" }\' > '
                  f"{CONFIG}\n\nOr pass the command directly: "
                  "record-gate.py -- <your gate>", file=sys.stderr)
            return 3
        args = list(gate_argv(declared))

    # A wrapped run of something that is not the declared gate is legal — the
    # `--` form is there for exactly that — but it produces a receipt the Stop
    # hook will refuse, and the useful place to say so is here, where the
    # command was chosen, rather than at the end of the next turn.
    if not want_gate:
        declared_here = gate_command(root)
        if declared_here is not None and not is_declared_gate(
                {"command": shlex.join(args)}, declared_here):
            print(f"record-gate: this is not the gate {CONFIG} declares, so "
                  "the receipt will not satisfy the Stop hook.\n"
                  f"  declared:  {declared_here}\n"
                  "  run it with --gate to record the declared gate instead.",
                  file=sys.stderr)

    # BEFORE the command runs — see the module docstring.
    before = fingerprint(root)

    # No shell HERE, in either form. Under `--` the command arrives as a list
    # the caller's own shell already split, and re-quoting it through `sh -c`
    # would turn a path with a space into two arguments that fail in a way
    # nobody would attribute to this wrapper. Under `--gate` the list is
    # `bash -c <the declared string>`, built by guard.gate_argv() from a value
    # that was a shell command to begin with — so there is still nothing being
    # re-quoted, which is the distinction that keeps both forms honest.
    try:
        rc = subprocess.run(args).returncode
    except (OSError, FileNotFoundError) as exc:
        print(f"record-gate: could not run {args[0]!r}: {exc}", file=sys.stderr)
        return 3

    path = os.path.join(root, RECEIPT)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    # Written whole and replaced atomically: a receipt truncated by a Ctrl-C
    # mid-write is unparseable, and unparseable is treated as absent, which
    # would silently discard a passing run.
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(
            {
                "fingerprint": before,
                "verdict": "pass" if rc == 0 else "fail",
                "exit_code": rc,
                # shlex.join, not " ".join: the latter renders
                # ["bash", "-c", "a && b"] as `bash -c a && b`, which is a
                # DIFFERENT command and the exact one #178 is about. A label
                # that reads back as the broken form is worse than no label,
                # and the Stop hook now compares this field.
                "command": shlex.join(args),
                **({"gate_command": declared} if declared is not None else {}),
            },
            fh,
            indent=2,
            sort_keys=True,
        )
        fh.write("\n")
    os.replace(tmp, path)

    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv))
