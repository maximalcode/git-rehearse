//! Where rehearsals live on disk, and what they are named.
//!
//! ```text
//! <cache root>/                     e.g. ~/.cache/git-rehearse
//! └── <repo id>/                    git-city-3f2a1c9d5b7e4a60
//!     └── <rehearsal id>/           1786248000-00
//!         ├── meta.json
//!         ├── no-hooks/             empty, on purpose (see sandbox)
//!         └── sandbox/              the shadow clone
//! ```
//!
//! Nothing here touches the filesystem: these are the naming and lookup rules,
//! kept pure so they can be tested without a home directory.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Escape hatch for tests and for anyone who wants rehearsals somewhere else.
/// Used verbatim as the cache root — no `git-rehearse` component is appended,
/// because a caller who names a directory means that directory.
pub const CACHE_ENV: &str = "GIT_REHEARSE_CACHE_DIR";

/// Our directory inside a shared cache root.
const DIR_NAME: &str = "git-rehearse";

/// The cache root for this machine and user.
///
/// Order: `GIT_REHEARSE_CACHE_DIR`, then `XDG_CACHE_HOME`, then the platform
/// default (`~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows,
/// `~/.cache` elsewhere).
///
/// # Errors
///
/// [`Error::NoCacheDir`] if the environment names no usable directory — which
/// in practice means no `HOME`.
pub fn root() -> Result<PathBuf> {
    root_from(
        std::env::var_os(CACHE_ENV).as_deref(),
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        platform_default().as_deref(),
    )
}

/// The environment-free core of [`root`], so the precedence rules are testable
/// without mutating the environment — which edition 2024 makes `unsafe`, and
/// this crate forbids `unsafe`.
fn root_from(
    override_dir: Option<&OsStr>,
    xdg_cache_home: Option<&OsStr>,
    platform_default: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    // The XDG spec says a relative XDG_CACHE_HOME is invalid and must be
    // ignored, not resolved against the working directory — a cache that
    // moves when you `cd` is worse than no cache.
    let base = xdg_cache_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| platform_default.map(PathBuf::from))
        .ok_or(Error::NoCacheDir)?;
    Ok(base.join(DIR_NAME))
}

#[cfg(target_os = "macos")]
fn platform_default() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join("Library/Caches").into_os_string())
}

#[cfg(windows)]
fn platform_default() -> Option<std::ffi::OsString> {
    std::env::var_os("LOCALAPPDATA")
}

#[cfg(not(any(target_os = "macos", windows)))]
fn platform_default() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache").into_os_string())
}

/// A stable per-repository directory name: a readable slug plus a hash of the
/// canonicalised path.
///
/// The slug is for the human who goes looking in their cache directory; the
/// hash is what actually distinguishes two checkouts of the same project.
/// Callers pass an already-canonicalised path — `~/dev/app` and
/// `~/dev/../dev/app` must not produce two cache directories.
#[must_use]
pub fn repo_id(canonical_repo: &Path) -> String {
    let slug: String = canonical_repo.file_name().map_or_else(String::new, |name| {
        name.to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .take(32)
            .collect()
    });
    let slug = slug.trim_matches('-');
    let hash = fnv1a64(canonical_repo.to_string_lossy().as_bytes());
    if slug.is_empty() {
        format!("repo-{hash:016x}")
    } else {
        format!("{slug}-{hash:016x}")
    }
}

/// FNV-1a, 64-bit.
///
/// Hand-rolled rather than [`std::hash::DefaultHasher`] because that hasher's
/// output is explicitly *not* stable across Rust releases: a toolchain bump
/// would rename every cache directory and orphan every kept rehearsal. Not a
/// cryptographic hash and not used as one — this only has to be stable and
/// collision-resistant enough for local directory names.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::{fnv1a64, repo_id, root_from};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    /// An absolute path in the form *this* platform recognises.
    ///
    /// `Path::is_absolute` is false for a unix-style path on Windows — a
    /// drive letter or a UNC prefix is what makes a path absolute there — and
    /// the XDG rule below turns on exactly that question. Hard-coding a unix
    /// path would test nothing on Windows except the hard-coding.
    #[cfg(windows)]
    const ABSOLUTE: &str = r"C:\Users\u\AppData\Local";
    #[cfg(not(windows))]
    const ABSOLUTE: &str = "/home/u/.cache";

    #[test]
    fn the_override_wins_and_is_used_verbatim() {
        let root = root_from(
            Some(OsStr::new("/tmp/rehearsals")),
            Some(OsStr::new("/home/u/.cache")),
            Some(OsStr::new("/home/u/.cache")),
        )
        .expect("override is usable");
        assert_eq!(root, PathBuf::from("/tmp/rehearsals"));
    }

    #[test]
    fn xdg_cache_home_gets_our_own_subdirectory() {
        let root =
            root_from(None, Some(OsStr::new(ABSOLUTE)), None).expect("XDG_CACHE_HOME is usable");
        assert_eq!(root, PathBuf::from(ABSOLUTE).join("git-rehearse"));
    }

    #[test]
    fn a_relative_xdg_cache_home_is_ignored_per_spec() {
        let root = root_from(
            None,
            Some(OsStr::new("relative/cache")),
            Some(OsStr::new("/home/u/Library/Caches")),
        )
        .expect("falls back to the platform default");
        assert_eq!(root, PathBuf::from("/home/u/Library/Caches/git-rehearse"));
    }

    #[test]
    fn an_empty_override_is_not_a_directory_name() {
        let root = root_from(Some(OsStr::new("")), Some(OsStr::new(ABSOLUTE)), None)
            .expect("empty override falls through");
        assert_eq!(root, PathBuf::from(ABSOLUTE).join("git-rehearse"));
    }

    #[test]
    fn no_environment_at_all_is_an_error_not_a_guess() {
        assert!(root_from(None, None, None).is_err());
    }

    #[test]
    fn repo_ids_are_stable_readable_and_path_specific() {
        let a = repo_id(Path::new("/Users/x/dev/git-city"));
        let b = repo_id(Path::new("/Users/x/other/git-city"));
        assert_eq!(a, repo_id(Path::new("/Users/x/dev/git-city")), "stable");
        assert_ne!(a, b, "same name, different checkout, different cache dir");
        assert!(a.starts_with("git-city-"), "{a}");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{a} must be safe as a directory name"
        );
    }

    #[test]
    fn awkward_directory_names_still_produce_usable_ids() {
        let dotted = repo_id(Path::new("/Users/x/.dotfiles"));
        assert!(dotted.starts_with("dotfiles-"), "{dotted}");
        let spaced = repo_id(Path::new("/Users/x/My Repo!"));
        assert!(spaced.starts_with("my-repo-"), "{spaced}");
        let root_dir = repo_id(Path::new("/"));
        assert!(root_dir.starts_with("repo-"), "{root_dir}");
    }

    #[test]
    fn fnv1a_matches_the_published_vectors() {
        // From the FNV reference: these two are the standard 64-bit checks.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
