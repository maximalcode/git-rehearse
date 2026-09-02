//! Checks a prospective checkout using Git's own worktree safety rules.
//!
//! There is no portable Rust predicate for this: Git's answer depends on its
//! index, ignore rules, sparse-checkout settings and the filesystem's path
//! semantics. This module therefore creates a disposable Git repository,
//! copies the current worktree into it, and asks `git checkout` to perform the
//! same prospective checkout. The repository is the adapter; collision policy
//! remains Git's policy.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

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
    let repo_objects = git_path(repo, "objects")?;
    let rehearsed_objects = git_path(rehearsed, "objects")?;
    let source_index = git_path(repo, "index")?;
    let object_format = git::run(repo, ["rev-parse", "--show-object-format"])?;
    let template = probe.join("template");
    fs::create_dir(&template).map_err(Error::io(&template))?;
    git::run(
        probe,
        [
            OsString::from("init"),
            OsString::from("--quiet"),
            git::flag_with_path("--template=", &template),
            OsString::from(format!("--object-format={object_format}")),
        ],
    )?;
    let alternates = std::env::join_paths([repo_objects, rehearsed_objects])
        .map_err(|error| Error::Sandbox(format!("cannot configure collision probe: {error}")))?;
    let env = [
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates),
        // `--template` controls init; this also prevents any inherited
        // template setting from being consulted by later git commands in the
        // probe.
        ("GIT_TEMPLATE_DIR", template.clone().into_os_string()),
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
    let probe_index = git_path(probe, "index")?;
    fs::copy(&source_index, &probe_index).map_err(Error::io(&probe_index))?;
    copy_sparse_config(repo, probe)?;
    clear_worktree(probe).map_err(Error::io(probe))?;
    copy_worktree(repo, probe, true).map_err(Error::io(repo))?;
    // Carried changes are expected to be replaced by the reset before their
    // separately rehearsed result is restored. Restore tracked paths to the
    // clean probe index, retaining only the untracked filesystem state.
    git::run_with_env(probe, ["checkout", "--quiet", "--", "."], &env)?;
    // Ignored files are still overwritten by `reset --hard`. Remove ignore
    // files from the disposable copy so checkout asks about those paths too;
    // the target checkout restores any tracked ignore files in the probe.
    remove_ignore_files(probe, Path::new(""), repo, rehearsed, &target)?;

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

fn clear_worktree(root: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        remove_path(
            &path,
            metadata.is_dir() && !metadata.file_type().is_symlink(),
        )?;
    }
    Ok(())
}

fn git_path(repo: &Path, name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(git::run(repo, ["rev-parse", "--git-path", name])?);
    Ok(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    })
}

fn copy_sparse_config(repo: &Path, probe: &Path) -> Result<()> {
    let sparse = git::run(
        repo,
        [
            "config",
            "--local",
            "--bool",
            "--get",
            "core.sparseCheckout",
        ],
    );
    if !matches!(sparse.as_deref(), Ok("true")) {
        return Ok(());
    }
    git::run(probe, ["config", "core.sparseCheckout", "true"])?;
    let source = git_path(repo, "info/sparse-checkout")?;
    let destination = git_path(probe, "info/sparse-checkout")?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(Error::io(parent))?;
    }
    fs::copy(source, destination.clone()).map_err(Error::io(&destination))?;
    for key in ["core.sparseCheckoutCone", "index.sparse"] {
        if let Ok(value) = git::run(repo, ["config", "--local", "--bool", "--get", key]) {
            git::run(probe, ["config", key, &value])?;
        }
    }
    Ok(())
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

fn remove_ignore_files(
    root: &Path,
    relative_root: &Path,
    repo: &Path,
    rehearsed: &Path,
    target: &str,
) -> Result<()> {
    for entry in fs::read_dir(root).map_err(Error::io(root))? {
        let entry = entry.map_err(Error::io(root))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(Error::io(&path))?;
        if metadata.is_dir() {
            if entry.file_name() != ".git" {
                remove_ignore_files(
                    &path,
                    &relative_root.join(entry.file_name()),
                    repo,
                    rehearsed,
                    target,
                )?;
            }
        } else if entry.file_name() == ".gitignore" {
            let relative = relative_root.join(entry.file_name());
            let relative = relative.to_str().ok_or_else(|| {
                Error::Sandbox("cannot inspect a non-UTF-8 ignore file path".to_owned())
            })?;
            let tracked = git::run(repo, ["ls-files", "--error-unmatch", "--", relative]).is_ok();
            if !tracked
                && !git::run(
                    rehearsed,
                    ["ls-tree", "-r", "--name-only", target, "--", relative],
                )?
                .is_empty()
            {
                return Err(Error::Refused(format!(
                    "applying this rehearsal would overwrite untracked file(s):\n\
                     {relative} is untracked but becomes tracked\n\
                     Keep them elsewhere or rehearse again from here."
                )));
            }
            fs::remove_file(path).map_err(Error::io(root))?;
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
