# Release artifacts

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
named by target, with archives, checksums, and `.smoke.json` evidence. Their
archive labels are `vV-test-COMMIT`, keeping them distinct from releases.
Manual runs against tags also remain test builds.

Every build packages first, verifies the checksum, extracts into a fresh
folder, checks the packaged version and documents, then executes the extracted
binary against a temporary real Git repository. A retained fast-forward merge
must leave original HEAD, files and index unchanged; explicit Apply by rehearsal
ID must install the exact reviewed commit and expected file bytes with a clean
index. A failure prevents that target's upload and blocks release publication.
The JSON evidence records the target, archive digest, tool/Git versions and result.

Only a push of a `v*` tag publishes a public release, after every target passes.
The tag must match the Cargo package version. Implementing or manually testing
this workflow does not create a tag or publish a release.

To reproduce packaging on a native target with Python 3.11+ and the pinned Rust
toolchain, run `cargo build --release --locked --target TARGET`, then
`python3 scripts/release-artifact.py TARGET vVERSION`. Outputs go to `dist/`.
