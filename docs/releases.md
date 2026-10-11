# Release artifacts

## v1.4.0 preparation and publication boundary

The next release is **v1.4.0**, prepared from `develop` after #120. The latest
published version is v1.3.0. The new public result revision, endpoint descriptors
and conditional Apply option add compatible CLI functionality; existing Apply
syntax and JSON schema 1 remain supported. A minor version follows the expanded
CLI release precedent in #115. This is not an MCP-server or v2.0 release.

The preparation PR references #119 without closing it. Completing these checks
and producing Actions artifacts does not publish a compatible tool bundle.
The repository's release route remains a separately authorized `develop` →
`main` promotion followed by a new `v1.4.0` tag on the approved `main` commit.
Existing tags must not move. The tag must match both Cargo version entries.

Before closing #119, verify the published archives and unpacked executable
checksums for all four targets, retain their exact source revision and native
smoke evidence, and give Git City R7 the actual release identity, artifact names
and digests. Test-build names and development overrides are not release pins.
Update the README publication status only when publication is verified.

## Platform mapping

Checked on 2026-09-16 against Git City's [v0.8.0 release assets](https://github.com/maximalcode/git-city/releases/tag/v0.8.0),
[release workflow](https://github.com/maximalcode/git-city/blob/c5f719cff16f5e816455e1c8dbbbdf150e2559b1/.github/workflows/release.yml)
and [installer configuration](https://github.com/maximalcode/git-city/blob/c5f719cff16f5e816455e1c8dbbbdf150e2559b1/electron-builder.yml).
Windows and Linux currently ship x64; macOS ships both architectures.

| Git City OS / architecture | git-rehearse target | Archive extension | Native test runner |
| --- | --- | --- | --- |
| Linux x64 (AppImage, amd64 deb) | `x86_64-unknown-linux-gnu` | `.tar.gz` | `ubuntu-latest` |
| Windows x64 (NSIS installer) | `x86_64-pc-windows-msvc` | `.zip` | `windows-latest` |
| macOS x64 (Intel DMG) | `x86_64-apple-darwin` | `.tar.gz` | `macos-15-intel` |
| macOS arm64 (Apple Silicon DMG) | `aarch64-apple-darwin` | `.tar.gz` | `macos-latest` |

Recheck this mapping when Git City changes its installer targets. These are
native packages, not universal binaries. Linux uses glibc, not musl; testing
on the listed runner does not establish compatibility with older distributions.

## Download and install

For version `V` (without the leading `v`) and target `T` from the table,
the archive is `git-rehearse-vV-T.tar.gz` (Unix) or `git-rehearse-vV-T.zip`
(Windows). The checksum is the archive filename plus `.sha256`. Both live at
`https://github.com/maximalcode/git-rehearse/releases/download/vV/`.
Do not infer the tool version from Git City's application version: the bundler
must pin the git-rehearse version explicitly. Older releases may lack Intel macOS.

Each archive contains one directory with the same name as the archive minus
its extension, containing `git-rehearse` (or `git-rehearse.exe`), `LICENSE`,
`README.md`, and these instructions as `INSTALL.md`.

Download the archive and its checksum into the same directory. The checksum
file uses the same ASCII format on every platform: lowercase SHA-256, two
spaces, archive basename, newline. Verify before extracting:

- Linux: `sha256sum -c ARCHIVE.sha256`.
- macOS: `shasum -a 256 -c ARCHIVE.sha256`.
- Windows PowerShell: compare `(Get-FileHash ARCHIVE.zip -Algorithm SHA256).Hash`
  with the first whitespace-separated field of `Get-Content ARCHIVE.zip.sha256`;
  stop if they differ (case-insensitive comparison).

On Unix, extract with `tar xzf ARCHIVE.tar.gz` and put `git-rehearse` in a
writable directory on `PATH`. On Windows, use `Expand-Archive ARCHIVE.zip`,
then put the extracted directory on `PATH`. Run `git-rehearse --version`.
Git must already be installed and on `PATH`; see README for its requirements.
Python and Rust are build/test tools, not runtime installation requirements.
For Git City bundling, retain the license and invoke the bundled executable
by its explicit path. This repository supplies the tool; application bundling
and packaged Electron acceptance tests belong to Git City.

## Test builds and publication

The release workflow builds all four targets on pull requests and manual
`workflow_dispatch` runs. These runs upload downloadable Actions artifacts
named by target, with archives, archive and executable checksums, and `.smoke.json`
evidence. Their archive labels are `vV-test-COMMIT`, keeping them distinct from releases.
Manual runs against tags also remain test builds.

Every build packages first, verifies the checksum, extracts into a fresh
folder, checks the packaged version and documents, then executes the extracted
binary against a temporary real Git repository. It starts simultaneous public
`--json --keep merge` previews from main and a linked worktree on distinct
branches sharing the same base; both must retain clean expected sandbox results,
report the correct main/linked administrative identities and distinct IDs, leave
both original HEADs, raw index bytes and file bytes unchanged, and remain visible
through `show`. The smoke repeats that pair to exercise startup contention. A
retained fast-forward merge must leave original HEAD, files and index unchanged;
public JSON must identify the result through a stable, versioned revision and
authoritative endpoint descriptors. Conditional Apply by rehearsal ID and that
revision must install the exact reviewed commit and expected file bytes with a
clean index. Missing, malformed and mismatched conditional arguments must refuse
without altering refs, HEAD, raw index bytes, worktree files, recovery/Undo data
or the retained result. A failure prevents that target's upload and blocks release
publication.
The extracted binary also migrates a retained, edited conflicting merge: it must
refuse a blocked backup, preserve the exact schema-1 bytes and optional annotation,
keep saved sandbox edits across repeated reads and retries, and refuse incompatible
metadata without rewriting or deletion.

Each target also builds the actual v1.3.0 source commit
`81a0621b4089749dfa899233a2a3ee06eacad44e` separately. That previous executable
creates real retained data which the unpacked candidate must read and use:

- Completed results acquire stable public revisions and endpoints, then accept
  conditional Apply of the exact reviewed result.
- Conflicted results preserve saved edits, continue with the new executable and
  require review of the completed result before Apply.
- Carried tracked work preserves its content through upgrade; Apply adopts the
  public replay endpoint's index and file content. The existing replay policy
  can leave originally staged changes unstaged. Recovery rollback restores the
  original staged and unstaged state.
- Interrupted recovery journals retain their snapshots and support the current
  recovery policy across the executable upgrade.

These fixtures run on every native target; changing a metadata schema number
alone is not the proof of a previous-version upgrade. The older schema migration
and unknown-schema preservation checks remain additional coverage.

Every `.smoke.json` records the exact checked-out `source_revision`, `target`,
`archive`, `archive_sha256`, `executable` and `executable_sha256`, tool/Git versions,
the pinned `previous_release` source and executable digest, and scenario results.
The older `sha256` field remains an alias for the archive digest. The
`ARCHIVE.binary.sha256` sidecar names the executable; verify it from the extracted
directory containing that executable, using the same checksum tools as above.
On a pull request, `source_revision` identifies the tested GitHub merge commit,
not necessarily the branch head. Test archive labels include that same revision.
Archive checksums cover the compressed download; executable checksums cover the
actual unpacked program. Preserve both when handing a published bundle to Git
City, together with the source revision and release identity.

The smoke evidence records `concurrent_worktree_previews: "passed"` alongside the
Apply and retained-metadata migration checks. Only a push of a `v*` tag publishes
a public release, after every target passes.
The tag must match the Cargo package version. Implementing or manually testing
this workflow does not create a tag or publish a release.

To reproduce packaging on a native target with Python 3.11+ and the pinned Rust
toolchain, build the candidate with `cargo build --release --locked --target
TARGET`. In a separate checkout of the pinned v1.3.0 commit, build that version
with the same command. Then run:

```bash
python3 scripts/release-artifact.py TARGET vVERSION-test-SOURCE_REVISION \
  --previous-binary /path/to/previous/target/TARGET/release/git-rehearse \
  --previous-source-revision 81a0621b4089749dfa899233a2a3ee06eacad44e
```

Use the `.exe` executable on Windows. `SOURCE_REVISION` is the candidate's full
`git rev-parse HEAD`. Outputs go to `dist/`. For v1.4.0 the eventual published
archive names are `git-rehearse-v1.4.0-TARGET.tar.gz` (Unix) or
`git-rehearse-v1.4.0-TARGET.zip` (Windows); they do not exist until the separately
authorized release succeeds.
