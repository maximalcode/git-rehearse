//! Durable worktree origin and shared branch occupancy checks.
use crate::{Error, Result, git};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
}

pub fn common_dir(repo: &Path) -> Result<PathBuf> {
    let path = git::run(
        repo,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    git::canonicalize(Path::new(&path))
}

impl Origin {
    pub fn capture(repo: &Path) -> Result<Self> {
        let git_dir = git::run(repo, ["rev-parse", "--absolute-git-dir"])?;
        Ok(Self {
            common_dir: common_dir(repo)?,
            git_dir: git::canonicalize(Path::new(&git_dir))?,
        })
    }

    pub fn verify(&self, repo: &Path) -> Result<()> {
        if Self::capture(repo).as_ref().is_ok_and(|now| now == self) {
            Ok(())
        } else {
            Err(Error::Refused("the rehearsal's original worktree or shared repository is unavailable or has changed; mutation is blocked".to_owned()))
        }
    }
}

/// NUL-delimited porcelain preserves paths containing whitespace and newlines.
/// Every registered non-bare worktree must still resolve to its own root in
/// this repository; missing/prunable entries cannot establish safe occupancy.
pub fn check_occupancy<'a>(repo: &Path, affected: impl Iterator<Item = &'a str>) -> Result<()> {
    let affected: Vec<_> = affected.collect();
    let origin = git::canonicalize(repo)?;
    let common = common_dir(repo)?;
    let main_origin = Origin::capture(repo)?.git_dir == common;
    let paths = registered_paths(repo, &origin, main_origin)?;
    let mut found_origin = 0;
    for path in paths {
        let top = git::run(&path, ["rev-parse", "--show-toplevel"]).map_err(|_| refusal())?;
        if git::canonicalize(Path::new(&top)).map_err(|_| refusal())? != path
            || common_dir(&path).map_err(|_| refusal())? != common
        {
            return Err(refusal());
        }
        if path == origin {
            found_origin += 1;
        }
        let current = symbolic_head(&path)?;
        let branch = current.as_deref();
        if path != origin && branch.is_some_and(|branch| affected.contains(&branch)) {
            return Err(Error::Refused(format!(
                "an affected branch is checked out in another worktree at {}; no branch or worktree was changed",
                path.display()
            )));
        }
        // Rebase and bisect detach HEAD while retaining ownership of the
        // branch they will restore. Porcelain's branch field omits that owner.
        let admin = Origin::capture(&path)?.git_dir;
        for state in [
            "rebase-merge/head-name",
            "rebase-apply/head-name",
            "BISECT_START",
        ] {
            let state_path = admin.join(state);
            match std::fs::read_to_string(&state_path) {
                Ok(owner) => {
                    let owner = owner.trim();
                    if owner.is_empty() {
                        return Err(refusal());
                    }
                    let branch = if owner.starts_with("refs/heads/") {
                        owner.to_owned()
                    } else {
                        format!("refs/heads/{owner}")
                    };
                    if path != origin && affected.contains(&branch.as_str()) {
                        return Err(refusal());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if state != "BISECT_START" && state_path.parent().is_some_and(Path::exists) {
                        return Err(refusal());
                    }
                }
                Err(_) => return Err(refusal()),
            }
        }
        if branch.is_none() {
            git::run(&path, ["rev-parse", "--verify", "HEAD"]).map_err(|_| refusal())?;
        }
    }
    if found_origin != 1 {
        return Err(refusal());
    }
    Ok(())
}

fn refusal() -> Error {
    Error::Refused("worktree occupancy or origin cannot be established safely; repair missing or ambiguous worktree registrations before mutating".to_owned())
}

/// Git holds each existing foreign HEAD in a prepared verification transaction.
/// The branch transaction then locks the origin HEAD and all affected refs.
/// Closing the pipes releases these native locks even if our process crashes.
pub fn transact(
    repo: &Path,
    commands: &str,
    affected: &[&str],
    message: &str,
    check: impl Fn() -> Result<()>,
) -> Result<()> {
    crate::test_hooks::pause("GIT_REHEARSE_PAUSE_TRANSACTION_AT", "before-head-locks");
    let origin = git::canonicalize(repo)?;
    let main_origin = Origin::capture(repo)?.git_dir == common_dir(repo)?;
    let paths = registered_paths(repo, &origin, main_origin)?;
    let mut guards = Vec::new();
    for path in &paths {
        if *path != origin {
            guards.push((path.clone(), head_verification(path)?));
        }
    }
    // Git implicitly locks HEAD when its referent participates in the
    // transaction, even for verification; adding HEAD again is rejected.
    let implicit_head = git::run(repo, ["symbolic-ref", "--quiet", "HEAD"])
        .is_ok_and(|target| commands.contains(&format!(" {target}\0")));
    let guarded = if implicit_head {
        commands.to_owned()
    } else {
        format!("{commands}{}", head_verification(repo)?)
    };
    lock_heads(&guards, &mut || {
        git::ref_transaction(repo, &guarded, message, || {
            check_occupancy(repo, affected.iter().copied())?;
            crate::test_hooks::pause("GIT_REHEARSE_PAUSE_TRANSACTION_AT", "after-occupancy");
            if registered_paths(repo, &origin, main_origin)? != paths {
                return Err(refusal());
            }
            check_occupancy(repo, affected.iter().copied())?;
            check()
        })
    })
}

fn head_verification(repo: &Path) -> Result<String> {
    if let Some(target) = symbolic_head(repo)? {
        Ok(format!("symref-verify HEAD\0{target}\0"))
    } else {
        let value = git::run(repo, ["rev-parse", "--verify", "HEAD"])?;
        Ok(format!("verify HEAD\0{value}\0"))
    }
}

fn lock_heads(
    guards: &[(PathBuf, String)],
    mutation: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let Some((path, command)) = guards.first() else {
        return mutation();
    };
    git::ref_transaction(path, command, "git-rehearse occupancy", || {
        lock_heads(&guards[1..], mutation)
    })
}

fn registered_paths(repo: &Path, origin: &Path, main_origin: bool) -> Result<Vec<PathBuf>> {
    let listing = git::run_bytes(repo, ["worktree", "list", "--porcelain", "-z"])?;
    let text = std::str::from_utf8(&listing).map_err(|_| refusal())?;
    let mut paths = Vec::new();
    for (position, record) in text
        .split("\0\0")
        .filter(|record| !record.is_empty())
        .enumerate()
    {
        if record.split('\0').any(|field| field == "bare") {
            continue;
        }
        let path = record
            .split('\0')
            .next()
            .and_then(|field| field.strip_prefix("worktree "))
            .ok_or_else(refusal)?;
        paths.push(if position == 0 && main_origin {
            origin.to_owned()
        } else {
            git::canonicalize(Path::new(path)).map_err(|_| refusal())?
        });
    }
    paths.sort();
    if paths.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(refusal());
    }
    Ok(paths)
}

fn symbolic_head(repo: &Path) -> Result<Option<String>> {
    match git::run(repo, ["symbolic-ref", "--quiet", "HEAD"]) {
        Ok(branch) => Ok(Some(branch)),
        Err(Error::Git { code: Some(1), .. }) => Ok(None),
        Err(error) => Err(error),
    }
}
