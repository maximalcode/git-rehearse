//! Checks a prospective checkout using Git's own worktree safety rules.
//!
//! There is no portable Rust predicate for this: Git's answer depends on its
//! index, ignore rules, sparse-checkout settings and the filesystem's path
//! semantics. This module therefore creates a disposable Git repository,
//! copies the current worktree into it, and asks `git checkout` to perform the
//! same prospective checkout. The repository is the adapter; collision policy
//! remains Git's policy.

use std::fs;
use std::path::Path;

use crate::{Error, Result, git};

/// Refuses if Git would overwrite an untracked path while checking out `target`.
///
/// `target` is a commit name resolvable in `rehearsed`; it may be the rehearsed
/// branch tip or the separate carried-result commit. No object or ref is
/// written to `repo`.
pub fn check(repo: &Path, rehearsed: &Path, target: &str) -> Result<()> {
    let parent = repo.parent().unwrap_or(repo);
    let temp = tempfile::Builder::new()
        .prefix(".git-rehearse-apply-")
        .tempdir_in(parent)
        .map_err(Error::io(parent))?;
    let probe = temp.path();
    let head = git::run(repo, ["rev-parse", "--verify", "HEAD"])?;
    let target = git::run(rehearsed, ["rev-parse", "--verify", target])?;
    git::run(probe, ["init", "--quiet"])?;
    let alternates =
        std::env::join_paths([repo.join(".git/objects"), rehearsed.join(".git/objects")]).map_err(
            |error| Error::Sandbox(format!("cannot configure collision probe: {error}")),
        )?;
    let env = [
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates),
        (
            "GIT_CONFIG_GLOBAL",
            temp.path().join("no-global-config").into_os_string(),
        ),
        (
            "GIT_CONFIG_SYSTEM",
            temp.path().join("no-system-config").into_os_string(),
        ),
    ];
    // Both commits are visible through object alternates, so no object is
    // fetched into the real repository.
    git::run_with_env(probe, ["checkout", "--quiet", "--detach", &head], &env)?;
    copy_worktree(repo, probe, true).map_err(Error::io(repo))?;
    // Carried changes are expected to be replaced by the reset before their
    // separately rehearsed result is restored. Restore tracked paths to the
    // clean probe index, retaining only the untracked filesystem state.
    git::run_with_env(probe, ["checkout", "--quiet", "--", "."], &env)?;
    // Ignored files are still overwritten by `reset --hard`. Remove ignore
    // files from the disposable copy so checkout asks about those paths too;
    // the target checkout restores any tracked ignore files in the probe.
    remove_ignore_files(probe).map_err(Error::io(probe))?;

    // This checkout updates only the disposable probe's HEAD and refs while
    // using Git's untracked-overwrite checks for every target path.
    match git::run_with_env(probe, ["checkout", "--quiet", "--detach", &target], &env) {
        Ok(_) => Ok(()),
        Err(Error::Git { stderr, .. }) => Err(Error::Refused(format!(
            "applying this rehearsal would overwrite untracked file(s):\n{stderr}\n\
             Keep them elsewhere or rehearse again from here."
        ))),
        Err(other) => Err(other),
    }
}

/// Copies the user's worktree over the clean probe checkout.
///
/// The recursive copy intentionally operates on `OsStr`/`Path` values. Git's
/// NUL-delimited path protocol permits names that are not UTF-8, and turning
/// those names into strings here would make a non-collision indistinguishable
/// from a collision.
fn copy_worktree(source: &Path, destination: &Path, top_level: bool) -> std::io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if top_level && name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = destination.join(&name);
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.is_dir() {
            if let Ok(existing) = fs::symlink_metadata(&to)
                && (!existing.is_dir() || existing.file_type().is_symlink())
            {
                remove_path(&to, existing.is_dir() && !existing.file_type().is_symlink())?;
            }
            fs::create_dir_all(&to)?;
            copy_worktree(&from, &to, false)?;
        } else if metadata.file_type().is_symlink() {
            remove_existing(&to)?;
            let directory = fs::metadata(&from).is_ok_and(|target| target.is_dir());
            symlink(&fs::read_link(&from)?, &to, directory)?;
        } else {
            remove_existing(&to)?;
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn remove_ignore_files(root: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            if entry.file_name() != ".git" {
                remove_ignore_files(&path)?;
            }
        } else if entry.file_name() == ".gitignore" {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn remove_existing(path: &Path) -> std::io::Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    remove_path(
        path,
        metadata.is_dir() && !metadata.file_type().is_symlink(),
    )
}

fn remove_path(path: &Path, directory: bool) -> std::io::Result<()> {
    if directory {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(unix)]
fn symlink(from: &Path, to: &Path, _directory: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(from, to)
}

#[cfg(windows)]
fn symlink(from: &Path, to: &Path, directory: bool) -> std::io::Result<()> {
    if directory {
        std::os::windows::fs::symlink_dir(from, to)
    } else {
        std::os::windows::fs::symlink_file(from, to)
    }
}
