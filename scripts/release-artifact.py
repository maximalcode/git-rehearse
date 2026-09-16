"""Package and smoke-test a release archive using only Python's standard library."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib


def run(args, cwd, env=None):
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True, timeout=120)
    if result.returncode:
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

    def rehearse(*args):
        return json.loads(run([str(binary), "--json", *args], repo, env))

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
    return {"version": observed_version, "git": git("--version"), "rehearsal_apply": "passed"}


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
