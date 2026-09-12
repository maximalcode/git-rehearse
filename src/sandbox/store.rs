//! Finding rehearsals again, and collecting the ones nobody wants.
//!
//! The cache is a two-level directory tree — `<cache>/<repo-id>/<rehearsal-id>`
//! — and everything here treats it as one: scan it, pick from it, remove from
//! it. Nothing in this file knows how a sandbox is built, and nothing in
//! [`super::build`] knows how one is found again.
//!
//! A directory with no metadata is skipped rather than reported. A foreign
//! directory, or one left by a run killed mid-clone, must not break a listing;
//! [`prune`] is what eventually collects it. Once metadata exists, a parse or
//! schema failure is surfaced as a refusal and preserved for diagnosis.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::Sandbox;
use super::meta::{Meta, Status};
use crate::recovery;
use crate::{Error, Result};

/// How long a transient rehearsal survives without being touched: seven days.
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
            let metadata = root.join("meta.json");
            match Meta::read(&root) {
                Ok(meta) => found.push(Sandbox { root, meta }),
                // A directory without metadata is a clone interrupted before
                // the durable record was written. It is not a rehearsal the
                // management API can identify, so leave it for prune.
                Err(_error) if matches!(metadata_state(&metadata), MetadataState::Absent) => {}
                // Once metadata exists, a parse or schema failure is an
                // identifiable but unsafe state. Refuse the listing instead
                // of silently dropping it and inviting a destructive guess.
                // A failed presence lookup is just as ambiguous as an entry
                // that is present: only a confirmed NotFound means orphaned.
                Err(error) => {
                    return Err(Error::Refused(format!(
                        "cannot safely read rehearsal metadata at {}: {error}",
                        metadata.display()
                    )));
                }
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
        .filter(|sandbox| matches_id(sandbox.id(), id))
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

/// Matches an exact rehearsal ID or an unambiguous shorthand candidate.
pub(crate) fn matches_id(rehearsal: &str, id: &str) -> bool {
    // Older linked IDs extended a complete main-worktree ID. Never resolve
    // that complete ID to a different rehearsal through prefix shorthand.
    let complete_main_id = id.split_once('-').is_some_and(|(timestamp, counter)| {
        !timestamp.is_empty()
            && timestamp.bytes().all(|byte| byte.is_ascii_digit())
            && counter.len() == 2
            && counter.bytes().all(|byte| byte.is_ascii_digit())
    });
    rehearsal == id || (!complete_main_id && rehearsal.starts_with(id))
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
            let metadata = root.join("meta.json");
            let meta = match Meta::read(&root) {
                Ok(meta) => Some(meta),
                // No metadata means a clone was interrupted before it could
                // be identified. Age pruning is the only cleanup available.
                Err(_) if matches!(metadata_state(&metadata), MetadataState::Absent) => None,
                // Unknown or corrupt metadata is protected: deleting it
                // would destroy the only record that could be migrated or
                // diagnosed later. An error checking whether metadata exists
                // is protected for the same reason.
                Err(_) => continue,
            };
            if meta
                .as_ref()
                .is_some_and(|meta| meta.status == Status::Kept)
            {
                continue;
            }
            let lock = if let Some(meta) = meta.as_ref() {
                // A repository can disappear or be moved after a rehearsal
                // was cached. Its sandbox is still useful evidence, so keep
                // the entry and let the other repositories in this shared
                // cache continue pruning. A live lock refusal remains an
                // error: hiding it could permit a concurrent mutation.
                match recovery::acquire(&meta.repo_path) {
                    Ok(lock) => Some(lock),
                    Err(_error) if !meta.repo_path.exists() => continue,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            if meta.as_ref().is_some_and(is_recovery_reserved) {
                continue;
            }
            // An interrupted apply may be the only durable link to the
            // sandbox's inspected result. Keep it until recovery has been
            // explicitly resolved, even when its normal cache TTL expired.
            if let Some(meta) = &meta {
                match recovery::inspect_locked(
                    &meta.repo_path,
                    lock.as_ref().expect("lock for metadata"),
                ) {
                    // A journal for this rehearsal remains the durable link
                    // to its result until recovery confirms completion.
                    Ok(Some(journal))
                        if journal.rehearsal == meta.id
                            && journal.state != recovery::State::Complete =>
                    {
                        continue;
                    }
                    // A damaged, incompatible, or otherwise uninspectable
                    // journal is itself an unresolved recovery state. Never
                    // delete the sandbox merely because we cannot parse it.
                    // A damaged journal, or one for another operation in the
                    // same repository, is ambiguous from this cache entry's
                    // perspective.
                    Err(_) | Ok(Some(_)) => continue,
                    Ok(None) => {}
                }
            }
            let created = match meta {
                Some(meta) => meta.created_unix,
                None => directory_mtime(&root)?,
            };
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
    pause_for_test("before-removal");
    let removed = match fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        // Already gone is the outcome the caller wanted.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::Io(root.to_path_buf(), err)),
    };
    pause_for_test("after-removal");
    removed
}

/// Test-only coordination seam for proving ownership survives actual removal.
fn pause_for_test(stage: &str) {
    crate::test_hooks::pause("GIT_REHEARSE_PAUSE_SANDBOX_REMOVAL_AT", stage);
}

/// Whether an apply journal has claimed this rehearsal for recovery.
///
/// The journal belongs to the real repository and is owned by the recovery
/// module. This boundary also recognizes opaque claims so cache
/// pruning and explicit discard cannot remove the rehearsal that recovery
/// still needs. A damaged journal is treated as reserved as well: failing
/// closed preserves the evidence for recovery to diagnose.
pub(super) fn is_recovery_reserved(meta: &Meta) -> bool {
    let Ok(journal) = recovery::path(&meta.repo_path) else {
        return true;
    };
    match fs::symlink_metadata(&journal) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        // Only a confirmed absence says there is no recovery owner. Existing
        // directories, symlinks, and other file types are ambiguous and must
        // be preserved for recovery to inspect.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Ok(_) | Err(_) => return true,
    }
    let Ok(text) = fs::read_to_string(&journal) else {
        return true;
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(document) => document
            .get("rehearsal")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|id| id == meta.id),
        Err(_) => true,
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

/// The only filesystem result that proves a metadata entry is absent.
///
/// `Path::exists` follows symlinks and collapses every lookup failure into
/// `false`. That would turn a damaged or unreadable metadata entry into an
/// apparently orphaned clone and let age pruning delete it. Inspect the
/// directory entry itself so broken symlinks and other failures stay visible.
#[derive(Debug)]
enum MetadataState {
    Present,
    Absent,
    Unknown,
}

fn metadata_state(path: &Path) -> MetadataState {
    match fs::symlink_metadata(path) {
        Ok(_) => MetadataState::Present,
        Err(error) if error.kind() == io::ErrorKind::NotFound => MetadataState::Absent,
        Err(_) => MetadataState::Unknown,
    }
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
