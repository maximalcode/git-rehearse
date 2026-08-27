#!/usr/bin/env python3
"""`Stop` hook: a session may not call it done on code the gate has not seen.

WHAT IT ENFORCES

At the end of every turn, the content that differs from HEAD is fingerprinted
and compared against the receipt written by `record-gate.py`. No receipt, a
stale one, or a failing one, and the stop is refused with an instruction to
run the gate. This is the rule CLAUDE.md files ask for and nothing enforces:
"run the checks before you say it is finished".

WHY IT IS ALSO THE BACKSTOP FOR THE OTHER HOOK

The docs are explicit that a tool matcher does not see everything: "Claude can
also create or modify files by running shell commands", and the recommended
answer is "a Stop hook that scans the working tree once per turn". So
`sample-guard.py` can be walked around with a heredoc, and this cannot —
whatever wrote the bytes, they are in `git status` at the end of the turn.
That is why the fixture asserts a Bash-written change is caught here.

THE LOOP GUARD IS NOT OPTIONAL

Claude Code overrides a Stop hook after it blocks eight times in a row, and a
hook that keeps blocking burns eight turns before the override. `stop_hook_active`
is true once this hook has already caused a continuation, and the documented
handling is to exit early. Without it, a repo whose gate cannot pass — a
broken toolchain, a pre-existing failure nobody has fixed yet — becomes a repo
where no session can end.
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from guard import (  # noqa: E402
    ALLOW,
    CONFIG,
    RECEIPT,
    block_stop,
    changed_files,
    is_declared_gate,
    fingerprint,
    gate_command,
    read_event,
    read_receipt,
    repo_root,
    warn,
)


def recorder(root: str) -> str:
    """Where record-gate.py is, as a path the reader can paste.

    Derived from this file's own location rather than hardcoded, because
    `adopt.sh --agent` copies these scripts to `.claude/agent-guard/` and the
    baseline runs them from `scripts/agent-guard/`. A hardcoded path is right
    in exactly one of those trees and names a nonexistent file in the other —
    which is a refusal whose remedy does not run, in the tree where this
    contract is doing the work it was written for.
    """
    here = os.path.dirname(os.path.abspath(__file__))
    rel = os.path.relpath(os.path.join(here, "record-gate.py"), root)
    # Outside the repo (a shared checkout, a symlinked install) `relpath` walks
    # up out of the tree; an absolute path is shorter to read and always right.
    return here + os.sep + "record-gate.py" if rel.startswith("..") else rel


def instruction(root: str) -> str:
    """What to run, named as concretely as this repo allows.

    `--gate` rather than `-- <the declared command>`, because this string is
    printed to be pasted and the interpolated form could not survive that.
    A gate written the natural way for two checks, `a && b`, produced

        python3 .../record-gate.py -- a && b

    whose `&&` binds OUTSIDE the wrapper: `b` then ran unrecorded and the
    receipt claimed a pass for it (#178). `--gate` takes no operators.

    The declared command is still named, on its own line. An instruction that
    hides what it is about to run is a different way of not saying it, and the
    fixture asserts both halves.

    THE UNDECLARED CASE PRINTS `--gate` TOO, AND THAT IS THE POINT

    It used to print `-- <the gate command your CLAUDE.md names>`, which is an
    interpolation slot handed to a human — the same trap one step further out,
    and the one a freshly adopted tree is in, since `--agent` cannot declare a
    gate for you. Filling that slot with `a && b` reproduces #178 exactly.
    `--gate` with nothing declared exits 3 and says what to write, which is a
    loud wrong answer instead of a quiet one.
    """
    cmd = gate_command(root)
    if cmd:
        return (f"python3 {recorder(root)} --gate\n\n"
                f"  (--gate runs the gate {CONFIG} declares: {cmd})")
    return (
        f"python3 {recorder(root)} --gate\n\n  (declare it first — this repo "
        f"has no {CONFIG}:\n   echo '{{ \"gate_command\": \"<your gate>\" }}' "
        f"> {CONFIG}\n   A gate that is two checks belongs in that string, not "
        "pasted onto this line.)"
    )


def main() -> int:
    event = read_event()
    if event is None:
        warn("stop: unreadable hook payload; allowing the stop")
        return ALLOW

    # Documented loop guard. Checked before anything else, so a repo that
    # cannot pass its own gate costs one blocked turn rather than eight.
    if event.get("stop_hook_active") is True:
        return ALLOW

    root = repo_root(event.get("cwd"))
    if root is None:
        warn("stop: not inside a git working tree; allowing the stop")
        return ALLOW

    changed = changed_files(root)
    if not changed:
        # Nothing differs from HEAD. There is no such thing as an ungated
        # change here, and a read-only session must not be made to run a gate.
        return ALLOW

    receipt = read_receipt(root)
    current = fingerprint(root)

    if receipt is None:
        block_stop(
            f"The gate has not run. {len(changed)} file(s) differ from HEAD and "
            f"there is no {RECEIPT}.\n\nRun it, then stop:\n\n  "
            f"{instruction(root)}\n\nIf it fails, fix what it reports — do not "
            "record a receipt by hand."
        )
        return ALLOW

    if receipt.get("verdict") != "pass":
        block_stop(
            "The last recorded gate run FAILED"
            + (f" ({receipt['command']})" if isinstance(receipt.get("command"), str) else "")
            + ".\n\nFix what it reported and run it again:\n\n  "
            f"{instruction(root)}"
        )
        return ALLOW

    declared = gate_command(root)
    if declared is not None and not is_declared_gate(receipt, declared):
        # Fresh and passing, for a command that is not the gate. The
        # fingerprint cannot see this: the content really was checked, by a
        # check that was only part of what this repo calls checking.
        recorded = receipt.get("command")
        block_stop(
            "The recorded gate run is not this repo's gate.\n\n"
            f"  recorded:  {recorded if isinstance(recorded, str) else '(nothing)'}\n"
            f"  declared:  {declared}  (in {CONFIG})\n\n"
            "Only the declared gate counts as the gate having run — a subset "
            "of it cannot stand in for the whole, and a superset is a different "
            f"claim. Run it, then stop:\n\n  {instruction(root)}"
        )
        return ALLOW

    if receipt.get("fingerprint") != current:
        block_stop(
            "The gate passed, but the working tree has changed since — the "
            "recorded verdict describes code that no longer exists.\n\nRun it "
            f"again, then stop:\n\n  {instruction(root)}"
        )
        return ALLOW

    return ALLOW


if __name__ == "__main__":
    sys.exit(main())
