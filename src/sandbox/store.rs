//! Finding rehearsals again, and collecting the ones nobody wants.
//!
//! The cache is a two-level directory tree — `<cache>/<repo-id>/<rehearsal-id>`
//! — and everything here treats it as one: scan it, pick from it, remove from
//! it. Nothing in this file knows how a sandbox is built, and nothing in
//! [`super::build`] knows how one is found again.
//!
//! A directory that does not parse as a rehearsal is consistently *skipped*
//! rather than reported. A foreign directory, or one left by a run killed
//! mid-clone, must not break a listing; [`prune`] is what eventually collects
//! it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::Sandbox;
use super::meta::Meta;
use crate::{Error, Result};

/// How long a kept rehearsal survives without being touched: seven days.
pub const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Every rehearsal in the cache, newest last.
///
/// Pass `repo_id` to limit the listing to one repository — that is what
/// `git rehearse list` inside a repo wants.
///
/// # Errors
///
/// [`Error::Io`] if the cache root exists but cannot be read. A cache root
/// that does not exist yet is not an error — it is an empty list.
pub fn list(cache_root: &Path, repo_id: Option<&str>) -> Result<Vec<Sandbox>> {
    let mut found = Vec::new();
    for repo_dir in subdirectories(cache_root)? {
        if let Some(wanted) = repo_id
            && repo_dir.file_name().is_none_or(|name| name != wanted)
        {
            continue;
        }
        for root in subdirectories(&repo_dir)? {
            if let Ok(meta) = Meta::read(&root) {
                found.push(Sandbox { root, meta });
            }
        }
    }
    found.sort_by(|a, b| {
        a.meta
            .created_unix
            .cmp(&b.meta.created_unix)
            .then_with(|| a.meta.id.cmp(&b.meta.id))
    });
    Ok(found)
}

/// Finds one rehearsal of a repository.
///
/// `id` may be a full rehearsal id or a unique prefix of one — nobody wants to
/// type `1786248000-00` from a listing. Without an id, the most recent
/// rehearsal of that repository is the one meant, because "apply it" said
/// straight after a rehearsal can only mean that one.
///
/// # Errors
///
/// [`Error::Refused`] if there is no such rehearsal, or if a prefix matches
/// more than one — an ambiguous id must not be resolved by guessing.
pub fn find(cache_root: &Path, repo_id: &str, id: Option<&str>) -> Result<Sandbox> {
    let mut candidates = list(cache_root, Some(repo_id))?;
    let Some(id) = id else {
        return candidates.pop().ok_or_else(|| {
            Error::Refused(
                "no rehearsals for this repository.\n\
                 Rehearse something first: `git rehearse merge <branch>`."
                    .to_owned(),
            )
        });
    };

    let mut matches: Vec<Sandbox> = candidates
        .into_iter()
        .filter(|sandbox| sandbox.id() == id || sandbox.id().starts_with(id))
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(Error::Refused(format!(
            "no rehearsal {id} for this repository.\n\
             `git rehearse list` shows the ones there are."
        ))),
        _ => Err(Error::Refused(format!(
            "{id} matches {} rehearsals.\n\
             Use more of the id — `git rehearse list` shows them in full.",
            matches.len()
        ))),
    }
}

/// Deletes rehearsals older than `max_age_secs`, returning the ids removed.
///
/// Age comes from `meta.json` where there is one and from the directory's
/// modification time where there is not — a run killed mid-clone leaves a
/// directory with no `meta.json`, and nothing else would ever collect it.
///
/// # Errors
///
/// [`Error::Io`] if the cache root cannot be read or an expired rehearsal
/// cannot be removed.
pub fn prune(cache_root: &Path, now_unix: u64, max_age_secs: u64) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for repo_dir in subdirectories(cache_root)? {
        for root in subdirectories(&repo_dir)? {
            let created = Meta::read(&root)
                .map(|meta| meta.created_unix)
                .or_else(|_| directory_mtime(&root))?;
            if now_unix.saturating_sub(created) <= max_age_secs {
                continue;
            }
            remove_rehearsal(&root)?;
            if let Some(name) = root.file_name() {
                removed.push(name.to_string_lossy().into_owned());
            }
        }
        // A repository directory with nothing left in it is noise in the
        // cache; it comes back by itself on the next rehearsal.
        let _ = fs::remove_dir(&repo_dir);
    }
    removed.sort();
    Ok(removed)
}

/// Immediate, complete removal of one rehearsal directory.
pub(super) fn remove_rehearsal(root: &Path) -> Result<()> {
    match fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        // Already gone is the outcome the caller wanted.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::Io(root.to_path_buf(), err)),
    }
}

/// The directories directly inside `dir`, sorted; empty if `dir` is absent.
fn subdirectories(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::Io(dir.to_path_buf(), err)),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::io(dir))?;
        if entry.file_type().map_err(Error::io(dir))?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// A directory's modification time, in seconds since the Unix epoch.
fn directory_mtime(dir: &Path) -> Result<u64> {
    let modified = fs::metadata(dir)
        .and_then(|meta| meta.modified())
        .map_err(Error::io(dir))?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs()))
}
