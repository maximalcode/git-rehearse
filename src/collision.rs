//! Checks a prospective checkout using Git's own worktree safety rules.
//!
//! There is no portable Rust predicate for this: Git's answer depends on its
//! index, ignore rules, sparse-checkout settings and the filesystem's path
//! semantics. This module therefore creates a disposable Git repository,
//! copies the current worktree into it, and asks Git to perform the same
//! prospective worktree operation. The repository is the adapter; collision
//! policy remains Git's policy.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result, git};

/// Checks the exact `git reset --hard <target>` operation used by apply.
///
/// `target` is a commit name resolvable in `rehearsed`; no object or ref is
/// written to `repo`.
pub fn check_reset(repo: &Path, rehearsed: &Path, target: &str) -> Result<()> {
    check_operation(repo, rehearsed, target, Operation::Reset)
}

/// Checks the exact `git read-tree -u --reset <target>^{tree}` operation used
/// to restore a carried result.
pub fn check_restore(repo: &Path, rehearsed: &Path, target: &str) -> Result<()> {
    check_operation(repo, rehearsed, target, Operation::Restore)
}

#[derive(Clone, Copy)]
enum Operation {
    Reset,
    Restore,
}

fn check_operation(
    repo: &Path,
    rehearsed: &Path,
    target: &str,
    operation: Operation,
) -> Result<()> {
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
    let alternates = std::env::join_paths([repo_objects, rehearsed_objects])
        .map_err(|error| Error::Sandbox(format!("cannot configure collision probe: {error}")))?;
    let env = [
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates),
        // Command-scoped configuration is supplied through an unbounded set
        // of environment variables, so git::run_with_clean_env removes all
        // of it before layering these values back in. In particular, hooks and
        // caller-provided excludes must not change the probe's answer.
        ("GIT_CONFIG_COUNT", OsString::from("0")),
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
    git::run_with_clean_env(
        probe,
        [
            OsString::from("init"),
            OsString::from("--quiet"),
            git::flag_with_path("--template=", &template),
            OsString::from(format!("--object-format={object_format}")),
        ],
        &env,
    )?;
    // Both commits are visible through object alternates, so no object is
    // fetched into the real repository.
    git::run_with_clean_env(probe, ["checkout", "--quiet", "--detach", &head], &env)?;
    let probe_index = git_path_with_env(probe, "index", &env)?;
    copy_index(&source_index, &probe_index)?;
    copy_sparse_config(repo, probe, &env)?;
    clear_worktree(probe).map_err(Error::io(probe))?;
    copy_worktree(repo, probe, true).map_err(Error::io(repo))?;
    // Ignored files are still overwritten by `reset --hard`. Remove ignore
    // files from the disposable copy so the reset asks about those paths too;
    // the target reset restores any tracked ignore files in the probe.
    remove_ignore_files(probe, Path::new(""), repo, rehearsed, &target)?;
    let empty_directories = empty_directories(probe).map_err(Error::io(probe))?;
    let before = worktree_files(probe).map_err(Error::io(probe))?;
    let tracked = tracked_paths(probe, &env)?;

    // This updates only the disposable probe while using Git's real worktree
    // transition for the operation apply will perform.
    let result = match operation {
        Operation::Reset => {
            git::run_with_clean_env(probe, ["reset", "--hard", "--quiet", &target], &env)
        }
        Operation::Restore => {
            let tree = format!("{target}^{{tree}}");
            git::run_with_clean_env(probe, ["read-tree", "-u", "--reset", &tree], &env)
        }
    };
    match result {
        Ok(_) if empty_directories.iter().any(|path| !path.is_dir()) => Err(Error::Refused(
            "applying this rehearsal would replace an empty untracked directory\n\
                 Keep it elsewhere or rehearse again from here."
                .to_owned(),
        )),
        Ok(_)
            if before.iter().any(|(path, state)| {
                let relative = path.strip_prefix(probe).unwrap_or(path);
                !tracked.iter().any(|tracked| tracked == relative)
                    && worktree_file(path).is_none_or(|after| after != *state)
            }) =>
        {
            Err(Error::Refused(
                "applying this rehearsal would overwrite untracked file(s)\n\
             Keep them elsewhere or rehearse again from here."
                    .to_owned(),
            ))
        }
        Ok(_) => Ok(()),
        Err(Error::Git { stderr, .. }) => Err(Error::Refused(format!(
            "applying this rehearsal would overwrite untracked file(s):\n{stderr}\n\
             Keep them elsewhere or rehearse again from here."
        ))),
        Err(other) => Err(other),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FileState {
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn worktree_files(root: &Path) -> std::io::Result<BTreeMap<PathBuf, FileState>> {
    let mut files = BTreeMap::new();
    collect_worktree_files(root, &mut files)?;
    Ok(files)
}

fn collect_worktree_files(
    root: &Path,
    files: &mut BTreeMap<PathBuf, FileState>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_worktree_files(&path, files)?;
        } else if metadata.file_type().is_symlink() {
            files.insert(path.clone(), FileState::Symlink(fs::read_link(path)?));
        } else {
            files.insert(path.clone(), FileState::File(fs::read(path)?));
        }
    }
    Ok(())
}

fn worktree_file(path: &Path) -> Option<FileState> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        return None;
    }
    if metadata.file_type().is_symlink() {
        Some(FileState::Symlink(fs::read_link(path).ok()?))
    } else {
        Some(FileState::File(fs::read(path).ok()?))
    }
}

fn tracked_paths(probe: &Path, env: &[(&str, OsString)]) -> Result<Vec<PathBuf>> {
    let output = git::run_bytes_with_clean_env(probe, ["ls-files", "-z", "--cached"], env)?;
    Ok(output
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(path_from_git_name)
        .collect())
}

fn path_from_git_name(name: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(name.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(name).into_owned())
    }
}

fn copy_index(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination).map_err(Error::io(destination))?;
    let Some(source_parent) = source.parent() else {
        return Ok(());
    };
    let Some(destination_parent) = destination.parent() else {
        return Ok(());
    };
    for entry in fs::read_dir(source_parent).map_err(Error::io(source_parent))? {
        let entry = entry.map_err(Error::io(source_parent))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("sharedindex.")
        {
            let target = destination_parent.join(entry.file_name());
            fs::copy(entry.path(), target.clone()).map_err(Error::io(&target))?;
        }
    }
    Ok(())
}

fn empty_directories(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    collect_empty_directories(root, &mut result)?;
    Ok(result)
}

fn collect_empty_directories(root: &Path, result: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            if fs::read_dir(&path)?.next().transpose()?.is_none() {
                result.push(path);
            } else if entry.file_name() != ".git" {
                collect_empty_directories(&path, result)?;
            }
        }
    }
    Ok(())
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
    git_path_with(repo, name, |args| git::run(repo, args))
}

fn git_path_with_env(repo: &Path, name: &str, env: &[(&str, OsString)]) -> Result<PathBuf> {
    git_path_with(repo, name, |args| git::run_with_clean_env(repo, args, env))
}

fn git_path_with<F>(repo: &Path, name: &str, run: F) -> Result<PathBuf>
where
    F: FnOnce([&str; 3]) -> Result<String>,
{
    let path = PathBuf::from(run(["rev-parse", "--git-path", name])?);
    Ok(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    })
}

fn copy_sparse_config(repo: &Path, probe: &Path, env: &[(&str, OsString)]) -> Result<()> {
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
    git::run_with_clean_env(probe, ["config", "core.sparseCheckout", "true"], env)?;
    let source = git_path(repo, "info/sparse-checkout")?;
    let destination = git_path_with_env(probe, "info/sparse-checkout", env)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(Error::io(parent))?;
    }
    fs::copy(source, destination.clone()).map_err(Error::io(&destination))?;
    for key in ["core.sparseCheckoutCone", "index.sparse"] {
        if let Ok(value) = git::run(repo, ["config", "--local", "--bool", "--get", key]) {
            git::run_with_clean_env(probe, ["config", key, &value], env)?;
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
