"""Exercise an upgrade from a retained rehearsal made by an older binary.

The release job imports :func:`upgrade_smoke` and supplies the two
executables.  This module deliberately drives only the public JSON CLI and
uses real temporary Git repositories.  It is kept independent of the Rust
test hooks so it can run against release artifacts.
"""

from __future__ import annotations

from contextlib import contextmanager
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
from typing import Iterator


_TIMEOUT = 90


def _run(
    args: list[str | os.PathLike[str]],
    cwd: Path,
    env: dict[str, str],
    expected: int | tuple[int, ...] = 0,
) -> subprocess.CompletedProcess[str]:
    """Run a real process with a bounded lifetime and useful failure output."""

    result = subprocess.run(
        [os.fspath(arg) for arg in args],
        cwd=cwd,
        env=env,
        text=True,
        capture_output=True,
        timeout=_TIMEOUT,
    )
    allowed = (expected,) if isinstance(expected, int) else expected
    if result.returncode not in allowed:
        command = " ".join(os.fspath(arg) for arg in args)
        raise RuntimeError(
            f"{command} exited {result.returncode}, expected {allowed}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def _git(repo: Path, env: dict[str, str], *args: str, expected: int | tuple[int, ...] = 0) -> str:
    return _run(["git", *args], repo, env, expected).stdout.strip()


def _git_bytes(repo: Path, env: dict[str, str], *args: str) -> bytes:
    result = subprocess.run(
        ["git", *args], cwd=repo, env=env, capture_output=True, timeout=_TIMEOUT
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} exited {result.returncode}: "
            f"{result.stderr.decode('utf-8', errors='replace')}"
        )
    return result.stdout


def _json_cli(
    binary: Path,
    repo: Path,
    env: dict[str, str],
    *args: str,
    expected: int | tuple[int, ...] = 0,
) -> dict:
    result = _run([binary, "--json", *args], repo, env, expected)
    try:
        document = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"{binary} produced non-JSON stdout for {args}:\n{result.stdout}\n"
            f"stderr:\n{result.stderr}"
        ) from error
    if not isinstance(document, dict):
        raise RuntimeError(f"{binary} produced a non-object JSON document: {document!r}")
    return document


def _environment(root: Path, repo: Path, cache_name: str) -> dict[str, str]:
    config = root / "gitconfig"
    if not config.exists():
        config.write_text("[init]\n\tdefaultBranch = main\n", encoding="utf-8")
    return {
        **os.environ,
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": str(config),
        "XDG_CACHE_HOME": str(root / "xdg-cache"),
        "GIT_REHEARSE_CACHE_DIR": str(root / "rehearsals" / cache_name),
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_EDITOR": "true",
        "GIT_SEQUENCE_EDITOR": "true",
    }


def _init_repo(root: Path, name: str, *, conflict: bool = False, dirty: bool = False) -> tuple[Path, dict[str, str]]:
    repo = root / name
    repo.mkdir(parents=True)
    env = _environment(root, repo, name)
    _git(repo, env, "init", "-b", "main")
    _git(repo, env, "config", "user.name", "Upgrade smoke")
    _git(repo, env, "config", "user.email", "upgrade-smoke@example.invalid")
    _git(repo, env, "config", "commit.gpgsign", "false")
    (repo / "base.txt").write_bytes(b"base\n")
    _git(repo, env, "add", "base.txt")
    _git(repo, env, "commit", "-m", "base")
    if conflict:
        _git(repo, env, "checkout", "-b", "conflicting")
        (repo / "base.txt").write_bytes(b"feature side\n")
        _git(repo, env, "commit", "-am", "feature side")
        _git(repo, env, "checkout", "main")
        (repo / "base.txt").write_bytes(b"main side\n")
        _git(repo, env, "commit", "-am", "main side")
    else:
        _git(repo, env, "checkout", "-b", "feature")
        (repo / "feature.txt").write_bytes(b"feature\n")
        _git(repo, env, "add", "feature.txt")
        _git(repo, env, "commit", "-m", "feature")
        _git(repo, env, "checkout", "main")
    if dirty:
        (repo / "staged.txt").write_bytes(b"initial staged\n")
        (repo / "unstaged.txt").write_bytes(b"initial unstaged\n")
        _git(repo, env, "add", "staged.txt", "unstaged.txt")
        _git(repo, env, "commit", "-m", "tracked files")
        (repo / "staged.txt").write_bytes(b"reviewed staged bytes\n")
        _git(repo, env, "add", "staged.txt")
        (repo / "unstaged.txt").write_bytes(b"reviewed unstaged bytes\n")
    return repo, env


def _state(repo: Path, env: dict[str, str], files: tuple[str, ...] = ()) -> dict:
    return {
        "head": _git(repo, env, "rev-parse", "HEAD"),
        "refs": _git(repo, env, "for-each-ref", "--format=%(refname)=%(objectname)", "refs/heads"),
        # Git refreshes stat-cache bytes while inspecting a dirty worktree.
        # write-tree captures the index's semantic contents, which is the
        # staging state the public CLI promises to preserve.
        "index": _git(repo, env, "write-tree"),
        "files": {name: (repo / name).read_bytes() for name in files},
        "status": _git(repo, env, "status", "--porcelain=v1"),
    }


def _assert_untouched(before: dict, repo: Path, env: dict[str, str], files: tuple[str, ...]) -> None:
    after = _state(repo, env, files)
    if after != before:
        raise RuntimeError(f"original repository changed before explicit apply:\nwas {before!r}\nnow {after!r}")


def _result(report: dict, label: str) -> tuple[str, Path]:
    revision = report.get("result_revision")
    endpoints = report.get("result_endpoints")
    sandbox = report.get("sandbox")
    if not isinstance(revision, str) or not revision or not isinstance(endpoints, dict):
        raise RuntimeError(f"{label} did not publish result revision/endpoints: {report}")
    if not isinstance(sandbox, str) or not sandbox:
        raise RuntimeError(f"{label} did not publish a sandbox: {report}")
    return revision, Path(sandbox)


def _tree_bytes(repo: Path, env: dict[str, str], tree: str) -> dict[str, bytes]:
    paths = _git(repo, env, "ls-tree", "-r", "--name-only", tree).splitlines()
    return {path: _git_bytes(repo, env, "show", f"{tree}:{path}") for path in paths}


def _endpoint_contract(
    report: dict,
    label: str,
    env: dict[str, str],
    expected_snapshot_tree: str | None = None,
) -> dict:
    endpoints = report.get("result_endpoints")
    results = endpoints.get("results") if isinstance(endpoints, dict) else None
    sandbox = Path(report["sandbox"]) if isinstance(report.get("sandbox"), str) else None
    if not isinstance(results, dict) or not results or sandbox is None:
        raise RuntimeError(f"{label} did not publish result endpoints: {report}")
    actual_refs = {
        "HEAD",
        *_git(sandbox, env, "for-each-ref", "--format=%(refname)", "refs/heads").splitlines(),
    }
    if set(results) != actual_refs:
        raise RuntimeError(f"{label} endpoint ref set differs: {set(results)} != {actual_refs}")
    for ref, expected in results.items():
        actual = _git(sandbox, env, "rev-parse", ref)
        if actual != expected:
            raise RuntimeError(f"{label} sandbox endpoint {ref} differs: {actual} != {expected}")
    carried = endpoints.get("carried")
    if carried is not None:
        if not isinstance(carried, dict) or set(carried.get("paths", ())) != {"staged.txt", "unstaged.txt"}:
            raise RuntimeError(f"{label} did not publish carried object endpoints: {report}")
        for field in ("snapshot", "snapshot_ref", "replay", "replay_ref"):
            if not isinstance(carried.get(field), str) or not carried[field]:
                raise RuntimeError(f"{label} carried endpoint {field} is unavailable: {report}")
        snapshot_tree = _git(sandbox, env, "rev-parse", f"{carried['snapshot']}^2^{{tree}}")
        if expected_snapshot_tree is not None and snapshot_tree != expected_snapshot_tree:
            raise RuntimeError(
                f"{label} snapshot^2 tree does not match the predecessor index: "
                f"{snapshot_tree} != {expected_snapshot_tree}"
            )
        index = _git(sandbox, env, "rev-parse", f"{carried['replay']}^2^{{tree}}")
        tree = f"{carried['replay']}^{{tree}}"
    elif expected_snapshot_tree is not None:
        raise RuntimeError(f"{label} omitted carried endpoints for a carried predecessor index")
    else:
        index = _git(sandbox, env, "rev-parse", f"{results['HEAD']}^{{tree}}")
        tree = f"{results['HEAD']}^{{tree}}"
    return {"head": results["HEAD"], "refs": results, "index": index, "files": _tree_bytes(sandbox, env, tree)}


def _assert_repo_contract(contract: dict, repo: Path, env: dict[str, str], label: str) -> None:
    if _git(repo, env, "rev-parse", "HEAD") != contract["head"]:
        raise RuntimeError(f"{label} HEAD differs from the reviewed endpoint")
    actual_refs = {
        "HEAD",
        *_git(repo, env, "for-each-ref", "--format=%(refname)", "refs/heads").splitlines(),
    }
    if set(contract["refs"]) != actual_refs:
        raise RuntimeError(f"{label} endpoint ref set differs: {set(contract['refs'])} != {actual_refs}")
    for ref, expected in contract["refs"].items():
        if _git(repo, env, "rev-parse", ref) != expected:
            raise RuntimeError(f"{label} endpoint {ref} differs from the reviewed result")
    if _git(repo, env, "write-tree") != contract["index"]:
        raise RuntimeError(f"{label} index differs from the reviewed endpoint tree")
    actual_paths = set(_git(repo, env, "ls-files").splitlines())
    if actual_paths != set(contract["files"]):
        raise RuntimeError(f"{label} tracked path set differs from the reviewed tree")
    for path, expected in contract["files"].items():
        if (repo / path).read_bytes() != expected:
            raise RuntimeError(f"{label} tracked file bytes differ for {path}")


def _old_completed(old: Path, root: Path, name: str) -> tuple[Path, dict[str, str], dict, Path, dict]:
    repo, env = _init_repo(root, name)
    before = _state(repo, env, ("base.txt",))
    report = _json_cli(old, repo, env, "--keep", "merge", "--no-edit", "feature")
    sandbox = Path(report["sandbox"])
    _assert_untouched(before, repo, env, ("base.txt",))
    return repo, env, report, sandbox, before


@contextmanager
def _git_transaction_failure(root: Path, base_env: dict[str, str]) -> Iterator[dict[str, str]]:
    """Run the real update-ref, then fail that public Git invocation once.

    The Rust process therefore observes a real Git failure after the ref
    transaction has committed.  This leaves the durable apply journal at the
    same boundary as a process dying after the transaction, without using a
    private test seam or faking Git's ref store.
    """

    wrapper_root = Path(tempfile.mkdtemp(prefix="git-wrapper-", dir=root))
    wrapper = wrapper_root / "git_wrapper.py"
    marker = wrapper_root / "triggered"
    wrapper.write_text(
        "import os, pathlib, subprocess, sys\n"
        "real = os.environ['UPGRADE_SMOKE_REAL_GIT']\n"
        "result = subprocess.run([real, *sys.argv[1:]])\n"
        "args = sys.argv[1:]\n"
        "if result.returncode == 0 and 'update-ref' in args and '--stdin' in args and not pathlib.Path(os.environ['UPGRADE_SMOKE_MARKER']).exists():\n"
        "    pathlib.Path(os.environ['UPGRADE_SMOKE_MARKER']).write_text('triggered', encoding='ascii')\n"
        "    raise SystemExit(73)\n"
        "raise SystemExit(result.returncode)\n",
        encoding="utf-8",
    )
    real_git = shutil.which("git", path=base_env.get("PATH"))
    if not real_git:
        raise RuntimeError("git was not found before installing the smoke wrapper")
    if os.name == "nt":
        # Rust's Command::new("git") resolves git.exe directly and does not
        # use cmd.exe's PATHEXT lookup.  Compile a tiny native proxy so the
        # Windows run exercises the same real-Git boundary as Unix.  The
        # release CI already has rustc because it builds the artifact.
        rustc = shutil.which("rustc", path=base_env.get("PATH"))
        if not rustc:
            raise RuntimeError("rustc is required for the Windows Git boundary wrapper")
        source = wrapper_root / "git_wrapper.rs"
        source.write_text(
            "use std::{env, fs, path::Path, process::{Command, Stdio}};\n"
            "fn main() {\n"
            "    let args: Vec<String> = env::args().skip(1).collect();\n"
            "    let real = env::var(\"UPGRADE_SMOKE_REAL_GIT\").expect(\"real git\");\n"
            "    let status = Command::new(real).args(&args).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status().expect(\"real git\");\n"
            "    let marker = env::var(\"UPGRADE_SMOKE_MARKER\").expect(\"marker\");\n"
            "    if status.success() && args.iter().any(|arg| arg == \"update-ref\") && args.iter().any(|arg| arg == \"--stdin\") && !Path::new(&marker).exists() {\n"
            "        fs::write(marker, b\"triggered\").expect(\"marker\");\n"
            "        std::process::exit(73);\n"
            "    }\n"
            "    std::process::exit(status.code().unwrap_or(1));\n"
            "}\n",
            encoding="utf-8",
        )
        launcher = wrapper_root / "git.exe"
        _run([rustc, source, "-O", "-o", launcher], root, base_env)
        path_entry = wrapper_root
    else:
        launcher = wrapper_root / "git"
        launcher.write_text(f"#!{sys.executable}\nexec(compile(open({str(wrapper)!r}).read(), {str(wrapper)!r}, 'exec'))\n", encoding="utf-8")
        launcher.chmod(launcher.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        path_entry = wrapper_root
    env = dict(base_env)
    env["UPGRADE_SMOKE_REAL_GIT"] = real_git
    env["UPGRADE_SMOKE_MARKER"] = str(marker)
    env["PATH"] = os.fspath(path_entry) + os.pathsep + env.get("PATH", "")
    try:
        yield env
    finally:
        shutil.rmtree(wrapper_root, ignore_errors=True)


def _scenario_completed(old: Path, current: Path, root: Path) -> dict:
    repo, env, report, _sandbox, before = _old_completed(old, root, "completed")
    metadata = Path(report["storage"]["metadata"])
    shown = _json_cli(current, repo, env, "show", report["id"])
    revision, _ = _result(shown, "current reload of old completed rehearsal")
    if shown.get("schema") != 1:
        raise RuntimeError(f"upgraded completed result changed the public JSON schema: {shown}")
    metadata_after_first_show = metadata.read_bytes()
    if json.loads(metadata_after_first_show).get("schema") != 3:
        raise RuntimeError("upgraded completed rehearsal did not retain schema-3 metadata")
    shown_again = _json_cli(current, repo, env, "show", report["id"])
    if (
        shown_again.get("result_revision") != revision
        or shown_again.get("result_endpoints") != shown.get("result_endpoints")
        or metadata.read_bytes() != metadata_after_first_show
    ):
        raise RuntimeError("repeated current reads changed the upgraded result or schema-3 metadata")
    contract = _endpoint_contract(shown, "current reload of old completed rehearsal", env)
    _assert_untouched(before, repo, env, ("base.txt",))
    applied = _json_cli(current, repo, env, "apply", report["id"], "--expected-result-revision", revision)
    if applied.get("result_revision") != revision:
        raise RuntimeError(f"current CLI applied a different result: {applied}")
    _assert_repo_contract(contract, repo, env, "completed old rehearsal apply")
    return {"status": "passed", "id": report["id"], "result_revision": revision}


def _scenario_conflict(old: Path, current: Path, root: Path) -> dict:
    repo, env = _init_repo(root, "conflict", conflict=True)
    before = _state(repo, env, ("base.txt",))
    stopped = _json_cli(old, repo, env, "--keep", "merge", "--no-edit", "conflicting", expected=2)
    if stopped.get("result_revision") is not None or stopped.get("result_endpoints") is not None:
        raise RuntimeError(f"old conflicting result unexpectedly advertised apply endpoints: {stopped}")
    sandbox = Path(stopped["sandbox"])
    (sandbox / "base.txt").write_bytes(b"reviewed conflict resolution\n")
    shown = _json_cli(current, repo, env, "show", stopped["id"])
    if shown.get("result_revision") is not None or shown.get("result_endpoints") is not None:
        raise RuntimeError(f"current CLI made a pending conflict applicable: {shown}")
    if (sandbox / "base.txt").read_bytes() != b"reviewed conflict resolution\n":
        raise RuntimeError("current show did not preserve a saved conflict resolution")
    _git(sandbox, env, "add", "base.txt")
    continued = _json_cli(current, repo, env, "--keep", "continue", stopped["id"])
    revision, _sandbox = _result(continued, "continued conflict rehearsal")
    contract = _endpoint_contract(continued, "continued conflict rehearsal", env)
    _assert_untouched(before, repo, env, ("base.txt",))
    _json_cli(current, repo, env, "apply", stopped["id"], "--expected-result-revision", revision)
    _assert_repo_contract(contract, repo, env, "continued conflict rehearsal apply")
    if (repo / "base.txt").read_bytes() != b"reviewed conflict resolution\n":
        raise RuntimeError("continued conflict resolution was not transplanted")
    return {"status": "passed", "id": stopped["id"], "result_revision": revision}


def _scenario_carried(old: Path, current: Path, root: Path) -> dict:
    repo, env = _init_repo(root, "carried", dirty=True)
    before = _state(repo, env, ("staged.txt", "unstaged.txt"))
    report = _json_cli(old, repo, env, "--keep", "merge", "--no-edit", "feature")
    shown = _json_cli(current, repo, env, "show", report["id"])
    revision, _sandbox = _result(shown, "current reload of old carried rehearsal")
    contract = _endpoint_contract(
        shown, "current reload of old carried rehearsal", env, before["index"]
    )
    _assert_untouched(before, repo, env, ("staged.txt", "unstaged.txt"))
    _json_cli(current, repo, env, "apply", report["id"], "--expected-result-revision", revision)
    _assert_repo_contract(contract, repo, env, "carried rehearsal apply")
    if (repo / "staged.txt").read_bytes() != b"reviewed staged bytes\n" or (repo / "unstaged.txt").read_bytes() != b"reviewed unstaged bytes\n":
        raise RuntimeError("carried tracked file bytes did not survive upgrade apply")
    status = _git(repo, env, "status", "--porcelain=v1")
    # Apply restores the reviewed index, while the carried file bytes are
    # intentionally left as worktree modifications (the product's carry
    # contract matches stash-pop without --index).
    status_paths = {line.strip().split()[-1] for line in status.splitlines() if line.strip()}
    worktree_paths = set(_git(repo, env, "diff", "--name-only").splitlines())
    if status_paths != {"staged.txt", "unstaged.txt"} or worktree_paths != status_paths:
        raise RuntimeError(f"carried staging and unstaged state changed: {status!r}")
    return {"status": "passed", "id": report["id"], "result_revision": revision}


def _scenario_incompatible(old: Path, current: Path, root: Path) -> dict:
    repo, env, report, sandbox, _before = _old_completed(old, root, "incompatible")
    metadata = Path(report["storage"]["metadata"])
    original = b'{"schema":999,"id":"preserve-me"}\n'
    metadata.write_bytes(original)
    refused = _json_cli(current, repo, env, "show", report["id"], expected=4)
    if "schema" not in refused.get("message", "").lower() or metadata.read_bytes() != original or not sandbox.exists():
        raise RuntimeError(f"incompatible metadata was not preserved/refused: {refused}")
    return {"status": "passed", "id": report["id"], "preserved": True}


def _scenario_recovery(old: Path, current: Path, root: Path) -> dict:
    clean_repo, clean_env, clean_report, _sandbox, clean_before = _old_completed(old, root, "recovery-clean")
    clean_shown = _json_cli(current, clean_repo, clean_env, "show", clean_report["id"])
    _result(clean_shown, "current reload of old clean recovery rehearsal")
    clean_contract = _endpoint_contract(clean_shown, "current reload of old clean recovery rehearsal", clean_env)
    with _git_transaction_failure(root, clean_env) as interrupted_env:
        # The predecessor owns the apply too: this is an on-disk recovery
        # journal migration, not merely a current-version crash test.
        failed = _run([old, "--json", "apply", clean_report["id"]], clean_repo, interrupted_env, expected=(1, 3, 4, 73))
    if not (clean_repo / ".git" / "rehearse-apply").exists():
        raise RuntimeError(f"interrupted clean apply did not retain its journal: {failed}")
    inspected = _json_cli(current, clean_repo, clean_env, "recover", clean_report["id"])
    if inspected.get("state") not in {"after_ref_change", "before_ref_change"} or not inspected.get("can_rollback"):
        raise RuntimeError(f"current CLI could not inspect old interrupted clean apply: {inspected}")
    _json_cli(current, clean_repo, clean_env, "recover", "--complete", clean_report["id"])
    if (clean_repo / ".git" / "rehearse-apply").exists() or not (clean_repo / ".git" / "rehearse-undo").exists():
        raise RuntimeError("clean recovery did not finish its journal/retain Undo snapshot")
    _assert_repo_contract(clean_contract, clean_repo, clean_env, "clean recovery completion")
    undo_status = _json_cli(current, clean_repo, clean_env, "undo", clean_report["id"], "--check")
    if not undo_status.get("available"):
        raise RuntimeError(f"completed clean recovery lost its Undo snapshot: {undo_status}")
    _json_cli(current, clean_repo, clean_env, "undo", clean_report["id"])
    if _state(clean_repo, clean_env, ("base.txt",)) != clean_before:
        raise RuntimeError("Undo after clean recovery did not restore the predecessor state")

    incompatible_repo, incompatible_env, incompatible_report, _sandbox, _before = _old_completed(
        old, root, "recovery-incompatible"
    )
    incompatible_shown = _json_cli(current, incompatible_repo, incompatible_env, "show", incompatible_report["id"])
    _result(incompatible_shown, "current reload of old incompatible-journal rehearsal")
    with _git_transaction_failure(root, incompatible_env) as interrupted_env:
        _run([old, "--json", "apply", incompatible_report["id"]], incompatible_repo, interrupted_env, expected=(1, 3, 4, 73))
    journal_path = incompatible_repo / ".git" / "rehearse-apply"
    journal = json.loads(journal_path.read_text(encoding="utf-8"))
    journal["schema"] = 999
    incompatible_bytes = (json.dumps(journal, separators=(",", ":")) + "\n").encode("utf-8")
    journal_path.write_bytes(incompatible_bytes)
    refused = _json_cli(current, incompatible_repo, incompatible_env, "recover", incompatible_report["id"], expected=4)
    message = refused.get("message", "").lower()
    if journal_path.read_bytes() != incompatible_bytes or not any(word in message for word in ("schema", "checksum", "journal")):
        raise RuntimeError(f"incompatible recovery journal was not preserved/refused: {refused}")

    carried_repo, carried_env = _init_repo(root, "recovery-carried", dirty=True)
    carried_before = _state(carried_repo, carried_env, ("staged.txt", "unstaged.txt"))
    carried_report = _json_cli(old, carried_repo, carried_env, "--keep", "merge", "--no-edit", "feature")
    carried_shown = _json_cli(current, carried_repo, carried_env, "show", carried_report["id"])
    _result(carried_shown, "current reload of old carried recovery rehearsal")
    _endpoint_contract(
        carried_shown,
        "current reload of old carried recovery rehearsal",
        carried_env,
        carried_before["index"],
    )
    with _git_transaction_failure(root, carried_env) as interrupted_env:
        _run([old, "--json", "apply", carried_report["id"]], carried_repo, interrupted_env, expected=(1, 3, 4, 73))
    recovered = _json_cli(current, carried_repo, carried_env, "recover", "--rollback", carried_report["id"])
    if recovered.get("state") not in {"rolled_back", "none"}:
        raise RuntimeError(f"carried recovery did not roll back: {recovered}")
    if _state(carried_repo, carried_env, ("staged.txt", "unstaged.txt")) != carried_before:
        raise RuntimeError("carried recovery rollback did not restore staged and unstaged bytes")
    return {
        "status": "passed",
        "clean": "complete+undo",
        "carried": "rollback",
        "incompatible_journal": "preserved+refused",
    }


def upgrade_smoke(previous_binary: str | os.PathLike[str], binary: str | os.PathLike[str], root: str | os.PathLike[str]) -> dict:
    """Run the release upgrade contract and return JSON-safe evidence."""

    previous = Path(previous_binary).resolve()
    current = Path(binary).resolve()
    root_path = Path(root).resolve()
    root_path.mkdir(parents=True, exist_ok=True)
    if not previous.is_file() or not current.is_file():
        raise RuntimeError(f"upgrade smoke binaries must be files: {previous}, {current}")
    probe_env = _environment(root_path, root_path, "version-probe")
    previous_version = _run([previous, "--version"], root_path, probe_env).stdout.strip()
    current_version = _run([current, "--version"], root_path, probe_env).stdout.strip()
    evidence = {
        "previous_version": previous_version,
        "current_version": current_version,
        "completed_result_apply": _scenario_completed(previous, current, root_path),
        "conflict_continue_apply": _scenario_conflict(previous, current, root_path),
        "carried_tracked_work": _scenario_carried(previous, current, root_path),
        "incompatible_metadata": _scenario_incompatible(previous, current, root_path),
        "recovery": _scenario_recovery(previous, current, root_path),
    }
    return evidence


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit(f"usage: {sys.argv[0]} PREVIOUS_BINARY CURRENT_BINARY ROOT")
    print(json.dumps(upgrade_smoke(sys.argv[1], sys.argv[2], sys.argv[3]), indent=2))


if __name__ == "__main__":
    main()
