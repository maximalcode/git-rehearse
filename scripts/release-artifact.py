"""Package and smoke-test a release archive using only Python's standard library."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib


def run(args, cwd, env=None, expected_code=0):
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True, timeout=120)
    if result.returncode != expected_code:
        raise RuntimeError(f"{args} exited {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result.stdout.strip()


def smoke(binary, version, root):
    env = dict(os.environ)
    env.update({
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": str(root / "gitconfig"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "GIT_REHEARSE_CACHE_DIR": str(root / "rehearsals"),
        "GIT_TERMINAL_PROMPT": "0",
    })
    repo = root / "repo"
    repo.mkdir()

    def git(*args):
        return run(["git", *args], repo, env)

    def rehearse(*args, expected_code=0):
        return json.loads(run([str(binary), "--json", *args], repo, env, expected_code))

    observed_version = run([str(binary), "--version"], repo, env)
    if observed_version != f"git-rehearse {version}":
        raise RuntimeError(f"Unexpected binary version: {observed_version}")
    git("init", "-b", "main")
    git("config", "user.name", "Release test")
    git("config", "user.email", "release-test@example.invalid")
    git("config", "commit.gpgsign", "false")
    (repo / "file.txt").write_bytes(b"before\n")
    git("add", "file.txt")
    git("commit", "-m", "Initial")
    before = git("rev-parse", "HEAD")
    git("checkout", "-b", "feature")
    (repo / "file.txt").write_bytes(b"after\n")
    git("commit", "-am", "Feature")
    expected = git("rev-parse", "HEAD")
    git("checkout", "main")
    concurrent_worktree_smoke(binary, root, repo, expected, env, before)
    preview = rehearse("--keep", "merge", "--ff-only", "feature")
    if not preview["can_apply"] or preview["decision"] != "kept":
        raise RuntimeError(f"Rehearsal was not retained and applicable: {preview}")
    reviewed = run(["git", "rev-parse", "HEAD"], Path(preview["sandbox"]), env)
    if reviewed != expected or git("rev-parse", "HEAD") != before:
        raise RuntimeError("Rehearsal changed the original or produced the wrong commit")
    if (repo / "file.txt").read_bytes() != b"before\n" or git("status", "--porcelain"):
        raise RuntimeError("Rehearsal changed original files or index")
    rehearse("apply", preview["id"])
    if git("rev-parse", "HEAD") != reviewed:
        raise RuntimeError("Apply did not transplant the reviewed commit")
    if (repo / "file.txt").read_bytes() != b"after\n" or git("status", "--porcelain"):
        raise RuntimeError("Apply left unexpected files or index")
    # Build a real stopped merge and retain a user's edited resolution.
    git("checkout", "-b", "conflicting")
    (repo / "file.txt").write_bytes(b"other branch\n")
    git("commit", "-am", "Other branch")
    git("checkout", "main")
    (repo / "file.txt").write_bytes(b"main branch\n")
    git("commit", "-am", "Main branch")
    stopped = rehearse("--keep", "merge", "conflicting", expected_code=2)
    migration_smoke(stopped, rehearse)
    return {"version": observed_version, "git": git("--version"),
            "rehearsal_apply": "passed", "retained_metadata_migration": "passed",
            "concurrent_worktree_previews": "passed"}


def concurrent_worktree_smoke(binary, root, repo, expected, env, expected_origin_head):
    """Exercise simultaneous public-CLI previews from two worktree origins."""

    linked = root / "linked"
    run(["git", "worktree", "add", "-b", "linked-base", str(linked), "main"], repo, env)

    def git_in(worktree, *args):
        return run(["git", *args], worktree, env)

    def admin_path(worktree):
        return Path(git_in(worktree, "rev-parse", "--absolute-git-dir"))

    def index_bytes(worktree):
        return admin_path(worktree).joinpath("index").read_bytes()

    origins = [repo, linked]
    initial_heads = [git_in(worktree, "rev-parse", "HEAD") for worktree in origins]
    if initial_heads != [expected_origin_head, expected_origin_head]:
        raise RuntimeError(f"Concurrent fixture did not start from the same base: {initial_heads}")
    initial_indexes = [index_bytes(worktree) for worktree in origins]
    initial_files = [(worktree / "file.txt").read_bytes() for worktree in origins]
    common_dir = Path(git_in(repo, "rev-parse", "--path-format=absolute", "--git-common-dir"))
    admin_paths = [admin_path(worktree) for worktree in origins]
    all_ids = set()

    # Repeating the pair makes startup lock contention observable while keeping
    # every invocation a real, simultaneous public-CLI process.
    for _ in range(3):
        processes = []
        streams = []
        try:
            for worktree in origins:
                stdout = tempfile.TemporaryFile()
                stderr = tempfile.TemporaryFile()
                streams.append((stdout, stderr))
                processes.append(subprocess.Popen(
                    [str(binary), "--json", "--keep", "merge", "feature"],
                    cwd=worktree,
                    env=env,
                    stdin=subprocess.DEVNULL,
                    stdout=stdout,
                    stderr=stderr,
                ))

            deadline = time.monotonic() + 120
            for process in processes:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(process.args, 120)
                process.wait(timeout=remaining)

            captures = []
            for process, (stdout, stderr) in zip(processes, streams):
                stdout.seek(0)
                stderr.seek(0)
                captures.append((
                    stdout.read().decode("utf-8", errors="replace"),
                    stderr.read().decode("utf-8", errors="replace"),
                ))
            failures = [
                (process.returncode, out, err)
                for process, (out, err) in zip(processes, captures)
                if process.returncode != 0
            ]
            if failures:
                raise RuntimeError(f"Concurrent preview failed: {failures}")
            previews = [json.loads(out) for out, _ in captures]
        finally:
            # A timeout or startup failure must never leave a release-smoke
            # child holding a repository/cache lock. Kill and reap every child
            # before the temporary fixture can be removed.
            for process in processes:
                try:
                    if process.poll() is None:
                        process.kill()
                except OSError:
                    # It exited between poll and kill; wait below still reaps it.
                    pass
            for process in processes:
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    try:
                        process.kill()
                    except OSError:
                        pass
                    process.wait(timeout=5)
            for stdout, stderr in streams:
                stdout.close()
                stderr.close()

        if len(previews) != len(origins):
            raise RuntimeError(f"Expected one preview per worktree: {previews}")
        ids = [preview.get("id") for preview in previews]
        if any(not rehearsal_id for rehearsal_id in ids) or ids[0] == ids[1] or any(
            rehearsal_id in all_ids for rehearsal_id in ids
        ):
            raise RuntimeError(f"Concurrent previews did not receive distinct IDs: {ids}")
        all_ids.update(ids)

        repository_ids = {preview.get("repository_id") for preview in previews}
        if len(repository_ids) != 1 or None in repository_ids:
            raise RuntimeError(f"Concurrent previews disagree on repository identity: {previews}")
        for preview, worktree, admin in zip(previews, origins, admin_paths):
            if (preview.get("outcome"), preview.get("decision"), preview.get("can_apply")) != (
                "clean", "kept", True
            ):
                raise RuntimeError(f"Concurrent preview was not a retained clean result: {preview}")
            if Path(preview["origin_worktree"]).resolve() != worktree.resolve():
                raise RuntimeError(f"Concurrent preview has the wrong origin: {preview}")
            metadata_path = Path(preview["storage"]["metadata"])
            metadata = json.loads(metadata_path.read_bytes())
            origin = metadata.get("origin")
            if not origin or Path(origin["common_dir"]).resolve() != common_dir.resolve():
                raise RuntimeError(f"Concurrent preview has the wrong common admin: {metadata}")
            if Path(origin["git_dir"]).resolve() != admin.resolve():
                raise RuntimeError(f"Concurrent preview has the wrong worktree admin: {metadata}")
            sandbox = Path(preview["sandbox"])
            if run(["git", "rev-parse", "HEAD"], sandbox, env) != expected:
                raise RuntimeError(f"Concurrent preview produced the wrong sandbox commit: {preview}")
            if (sandbox / "file.txt").read_bytes() != b"after\n":
                raise RuntimeError("Concurrent preview produced the wrong sandbox file bytes")
            shown = json.loads(run([str(binary), "--json", "show", preview["id"]], worktree, env))
            if (
                shown.get("id") != preview["id"]
                or shown.get("decision") != "kept"
                or shown.get("origin_worktree") != preview["origin_worktree"]
                or shown.get("repository_id") != preview["repository_id"]
            ):
                raise RuntimeError(f"Retained concurrent preview was not visible via show: {shown}")

    for worktree, head, index, file_bytes in zip(origins, initial_heads, initial_indexes, initial_files):
        if git_in(worktree, "rev-parse", "HEAD") != head:
            raise RuntimeError("Concurrent previews changed an original HEAD")
        if (worktree / "file.txt").read_bytes() != file_bytes:
            raise RuntimeError("Concurrent previews changed original file bytes")
        if git_in(worktree, "status", "--porcelain"):
            raise RuntimeError("Concurrent previews left an original worktree dirty")
        if index_bytes(worktree) != index:
            raise RuntimeError("Concurrent previews changed an original index")


def migration_smoke(preview, rehearse):
    metadata = Path(preview["storage"]["metadata"])
    backup = metadata.with_name("meta.json.bak")
    edited = Path(preview["sandbox"]) / "file.txt"
    edited.write_bytes(b"saved conflict resolution\n")
    legacy = json.loads(metadata.read_bytes())
    legacy["schema"] = 1
    legacy.pop("carry", None)
    legacy.pop("origin", None)
    legacy["optional_annotation"] = {"notes": [1, None, True]}
    original = (json.dumps(legacy, indent=2) + "\n").encode()
    metadata.write_bytes(original)
    backup.mkdir()
    failure = rehearse("show", preview["id"], expected_code=4)
    if "cannot preserve original metadata" not in failure["message"] or metadata.read_bytes() != original:
        raise RuntimeError("Failed backup did not protect original metadata")
    backup.rmdir()
    for _ in range(2):
        rehearse("show", preview["id"])
        migrated = json.loads(metadata.read_bytes())
        if migrated["schema"] != 3 or migrated["optional_annotation"] != legacy["optional_annotation"]:
            raise RuntimeError("Migration lost optional metadata")
        if backup.read_bytes() != original or edited.read_bytes() != b"saved conflict resolution\n":
            raise RuntimeError("Migration lost original metadata or saved sandbox edits")
    # Simulate retry after the original was preserved but migration did not commit.
    metadata.write_bytes(original)
    rehearse("show", preview["id"])
    if backup.read_bytes() != original:
        raise RuntimeError("Retry replaced the original backup")
    incompatible = b'{"schema":999,"id":"preserve-me"}\n'
    metadata.write_bytes(incompatible)
    rehearse("show", preview["id"], expected_code=4)
    if metadata.read_bytes() != incompatible or backup.read_bytes() != original or not edited.exists():
        raise RuntimeError("Incompatible metadata was rewritten or removed")


def main():
    target, label = sys.argv[1:]
    version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    if label != f"v{version}" and not label.startswith(f"v{version}-test-"):
        raise RuntimeError("Artifact label must match the Cargo package version")
    windows = target.endswith("windows-msvc")
    executable = "git-rehearse.exe" if windows else "git-rehearse"
    name = f"git-rehearse-{label}-{target}"
    output = Path("dist").resolve()
    output.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="release-package-") as directory:
        root = Path(directory)
        staging = root / name
        staging.mkdir()
        shutil.copy2(Path("target") / target / "release" / executable, staging)
        for document in ["README.md", "LICENSE"]:
            shutil.copy2(document, staging)
        shutil.copy2("docs/releases.md", staging / "INSTALL.md")
        archive = Path(shutil.make_archive(str(output / name), "zip" if windows else "gztar", root, name))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        checksum = archive.with_name(archive.name + ".sha256")
        checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
        recorded_digest, recorded_name = checksum.read_text().split()
        if recorded_name != archive.name or hashlib.sha256(archive.read_bytes()).hexdigest() != recorded_digest:
            raise RuntimeError("Archive checksum verification failed")
        unpacked = root / "unpacked"
        shutil.unpack_archive(archive, unpacked)
        package = unpacked / name
        for document in ["README.md", "LICENSE", "INSTALL.md"]:
            if not (package / document).is_file():
                raise RuntimeError(f"Missing packaged document: {document}")
        evidence = smoke(package / executable, version, root)
        evidence.update({"target": target, "archive": archive.name, "sha256": digest})
        (output / f"{name}.smoke.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2))


if __name__ == "__main__":
    main()
