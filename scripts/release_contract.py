"""Smoke-test the public reviewed-result Apply contract of a release binary."""

import json
import os
from pathlib import Path
import re
import subprocess


_REVISION = re.compile(r"^rr1:(?:[0-9a-f]{40}|[0-9a-f]{64})$")


def _run(args, cwd, env, expected_code=0):
    result = subprocess.run(
        args,
        cwd=cwd,
        env=env,
        text=True,
        capture_output=True,
        timeout=120,
    )
    if result.returncode != expected_code:
        raise RuntimeError(
            f"{args} exited {result.returncode}\n{result.stdout}\n{result.stderr}"
        )
    return result.stdout.strip()


def _json(binary, cwd, env, args, expected_code=0):
    output = _run([str(binary), "--json", *args], cwd, env, expected_code)
    try:
        return json.loads(output)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"{binary} did not print one JSON document: {output}") from error


def _state(repo, env):
    refs = {}
    for line in _run(
        ["git", "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads/"],
        repo,
        env,
    ).splitlines():
        name, sha = line.split(" ", 1)
        refs[name] = sha
    refs["HEAD"] = _run(["git", "rev-parse", "HEAD"], repo, env)
    return refs


def _all_files(root):
    files = {}
    for path in root.rglob("*"):
        if ".git" in path.relative_to(root).parts:
            continue
        if path.is_file():
            files[path.relative_to(root)] = path.read_bytes()
    return tuple(sorted(files.items()))


def _protected_state(repo, metadata, env):
    git_dir = Path(_run(["git", "rev-parse", "--absolute-git-dir"], repo, env))

    def read_optional(path):
        return path.read_bytes() if path.is_file() else None

    return {
        "refs": tuple(sorted(_state(repo, env).items())),
        "head": (git_dir / "HEAD").read_bytes(),
        "index": (git_dir / "index").read_bytes(),
        "files": _all_files(repo),
        "recovery": read_optional(git_dir / "rehearse-apply"),
        "undo": read_optional(git_dir / "rehearse-undo"),
        "metadata": metadata.read_bytes(),
    }


def _assert_endpoint_descriptions(report, repo, sandbox, env):
    revision = report.get("result_revision")
    if not isinstance(revision, str) or not _REVISION.fullmatch(revision):
        raise RuntimeError(f"retained report did not publish an opaque rr1 revision: {report}")
    endpoints = report.get("result_endpoints")
    if not isinstance(endpoints, dict) or endpoints.get("version") != 1:
        raise RuntimeError(f"retained report did not publish versioned endpoints: {report}")

    origin = endpoints.get("origin")
    if not isinstance(origin, dict):
        raise RuntimeError(f"result endpoints have no authoritative origin: {endpoints}")
    common_dir = _run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], repo, env
    )
    git_dir = _run(["git", "rev-parse", "--absolute-git-dir"], repo, env)
    for key, expected in {
        "rehearsal": report.get("id"),
        "repository_id": report.get("repository_id"),
    }.items():
        if origin.get(key) != expected:
            raise RuntimeError(f"endpoint origin {key} is not authoritative: {origin}")
    for key, expected in {
        "repository": repo,
        "common_dir": Path(common_dir),
        "git_dir": Path(git_dir),
    }.items():
        if Path(origin.get(key, "")).resolve() != expected.resolve():
            raise RuntimeError(f"endpoint origin {key} is not authoritative: {origin}")

    for key in ["action", "checkout", "pre_state", "results"]:
        if key not in endpoints:
            raise RuntimeError(f"endpoint description omitted {key}: {endpoints}")
    if endpoints["action"] != report.get("command"):
        raise RuntimeError(f"endpoint action disagrees with public report: {endpoints}")
    if endpoints["checkout"] != report.get("checkout"):
        raise RuntimeError(f"endpoint checkout disagrees with public report: {endpoints}")
    if endpoints["pre_state"] != report.get("pre_state"):
        raise RuntimeError(f"endpoint pre-state disagrees with public report: {endpoints}")
    if endpoints["results"] != _state(sandbox, env):
        raise RuntimeError(f"endpoint results do not describe the sandbox state: {endpoints}")
    if "carried" in endpoints:
        raise RuntimeError(f"clean smoke result unexpectedly published carried endpoints: {endpoints}")
    return revision, endpoints


def _assert_unchanged(repo, metadata, before, env):
    after = _protected_state(repo, metadata, env)
    if after != before:
        raise RuntimeError("conditional Apply refusal mutated refs, HEAD, index, files, recovery, Undo, or metadata")


def conditional_apply_smoke(binary, root):
    """Prove public result reads and strict conditional Apply on an unpacked binary."""

    env = dict(os.environ)
    env.update(
        {
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": str(root / "gitconfig-conditional"),
            "XDG_CACHE_HOME": str(root / "cache-conditional"),
            "GIT_REHEARSE_CACHE_DIR": str(root / "rehearsals-conditional"),
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    repo = root / "conditional-apply-repo"
    repo.mkdir()

    def git(*args):
        return _run(["git", *args], repo, env)

    git("init", "-b", "main")
    git("config", "user.name", "Release contract test")
    git("config", "user.email", "release-contract@example.invalid")
    git("config", "commit.gpgsign", "false")
    (repo / "file.txt").write_bytes(b"before\n")
    git("add", "file.txt")
    git("commit", "-m", "Initial")
    git("checkout", "-b", "feature")
    (repo / "file.txt").write_bytes(b"after\n")
    git("commit", "-am", "Feature")
    expected_head = git("rev-parse", "HEAD")
    git("checkout", "main")

    report = _json(binary, repo, env, ["--keep", "merge", "--ff-only", "feature"])
    if report.get("decision") != "kept" or not report.get("can_apply"):
        raise RuntimeError(f"conditional contract fixture was not retained: {report}")
    sandbox = Path(report.get("sandbox", ""))
    metadata = sandbox.parent / "meta.json"
    if not sandbox.is_dir() or not metadata.is_file():
        raise RuntimeError(f"retained contract fixture has no durable result: {report}")
    revision, endpoints = _assert_endpoint_descriptions(report, repo, sandbox, env)

    # `show` and `list` are public reads. Repeating both catches a reader that
    # recomputes a moving or mutable result instead of returning frozen data.
    metadata_before_reads = metadata.read_bytes()
    for _ in range(2):
        shown = _json(binary, repo, env, ["show", report["id"]])
        if shown.get("result_revision") != revision or shown.get("result_endpoints") != endpoints:
            raise RuntimeError(f"show changed the reviewed result: {shown}")
        listing = _json(binary, repo, env, ["list"])
        entries = [entry for entry in listing.get("rehearsals", []) if entry.get("id") == report["id"]]
        if len(entries) != 1 or entries[0].get("result_revision") != revision:
            raise RuntimeError(f"list did not return the stable reviewed result: {listing}")
        if entries[0].get("result_endpoints") != endpoints:
            raise RuntimeError(f"list endpoint descriptions changed: {listing}")
    if metadata.read_bytes() != metadata_before_reads:
        raise RuntimeError("public result reads rewrote retained metadata")

    # Put existing recovery records in place so every refusal proves it did
    # not replace or remove those records either. They are removed before the
    # successful Apply below.
    git_dir = Path(git("rev-parse", "--absolute-git-dir"))
    recovery = git_dir / "rehearse-apply"
    undo = git_dir / "rehearse-undo"
    recovery.write_bytes(b"release-contract recovery sentinel\n")
    undo.write_bytes(b"release-contract undo sentinel\n")

    before = _protected_state(repo, metadata, env)
    missing = _json(
        binary,
        repo,
        env,
        ["apply", report["id"], "--expected-result-revision"],
        expected_code=4,
    )
    if missing.get("kind") != "refused" or "needs a revision" not in missing.get("message", ""):
        raise RuntimeError(f"missing conditional revision was not refused clearly: {missing}")
    _assert_unchanged(repo, metadata, before, env)

    malformed = _json(
        binary,
        repo,
        env,
        ["apply", report["id"], "--expected-result-revision", "malformed"],
        expected_code=4,
    )
    if malformed.get("kind") != "refused" or "malformed" not in malformed.get("message", ""):
        raise RuntimeError(f"malformed conditional revision was not refused clearly: {malformed}")
    _assert_unchanged(repo, metadata, before, env)

    wrong_revision = "rr1:" + "0" * (len(revision) - len("rr1:"))
    mismatch = _json(
        binary,
        repo,
        env,
        ["apply", report["id"], "--expected-result-revision", wrong_revision],
        expected_code=4,
    )
    if mismatch.get("kind") != "refused" or "refresh" not in mismatch.get("message", ""):
        raise RuntimeError(f"mismatched conditional revision was not refused clearly: {mismatch}")
    _assert_unchanged(repo, metadata, before, env)

    recovery.unlink()
    undo.unlink()
    applied = _json(
        binary,
        repo,
        env,
        ["apply", report["id"], "--expected-result-revision", revision],
    )
    if applied.get("result_revision") != revision or applied.get("result_endpoints") != endpoints:
        raise RuntimeError(f"conditional Apply did not report the reviewed result: {applied}")
    if git("rev-parse", "HEAD") != expected_head:
        raise RuntimeError("conditional Apply did not transplant the reviewed commit")
    if (repo / "file.txt").read_bytes() != b"after\n" or git("status", "--porcelain"):
        raise RuntimeError("conditional Apply left unexpected files or index state")
    if (git_dir / "rehearse-undo").is_file() is False:
        raise RuntimeError("successful conditional Apply did not record Undo evidence")
    return {
        "public_result_revision": "passed",
        "authoritative_endpoints": "passed",
        "stable_reads": "passed",
        "conditional_apply_success": "passed",
        "conditional_apply_refusals": "passed",
    }
