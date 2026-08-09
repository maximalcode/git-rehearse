//! Moving the real repository's refs to the rehearsed commits.
//!
//! **Design principle 2, which CLAUDE.md marks inviolable: apply is a ref
//! transplant, never a re-run.** The objects the user inspected in the report
//! are fetched into the real repository and its refs are pointed at those exact
//! commit ids. Re-running the rehearsed command here would produce different
//! commits — different timestamps, different committer, a different conflict
//! resolution — and the report would have been a description of something the
//! user never got. The day apply re-runs a command, the tool's one guarantee
//! dies.
//!
//! The order is chosen so that nothing is half-done:
//!
//! 1. Check the repository is still the one that was rehearsed.
//! 2. Fetch the sandbox's objects, which changes no ref the user can see.
//! 3. Write the undo record — *before* moving anything, so a crash mid-apply
//!    still leaves the way back written down. It records both sides of every
//!    move, because `git rehearse undo` needs the values this transaction is
//!    about to write in order to state them back as expected old values; see
//!    [`crate::undo`], which owns the format.
//! 4. One `update-ref` transaction, with every expected old value stated, so
//!    git itself refuses the whole batch if anything moved underneath us.
//! 5. Only then touch the worktree.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::analyze::{RefMove, ref_moves};
use crate::preflight::HEAD_KEY;
use crate::sandbox::{Checkout, Sandbox};
use crate::undo::{self, Record};
use crate::{Error, Result, git};

/// What an apply did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The refs that were moved, created or deleted.
    pub moved: Vec<RefMove>,
    /// The branch whose worktree was reset, if the rewritten branch was the
    /// one checked out.
    pub reset: Option<String>,
    /// Where the pre-state was written down.
    pub undo: PathBuf,
    /// The namespace the rehearsed commits were fetched into, kept as an
    /// anchor so nothing that was just applied can be garbage-collected.
    pub anchor: String,
}

/// Applies a rehearsal to the repository it was rehearsed against.
///
/// # Errors
///
/// [`Error::Refused`] if the repository is no longer the one that was
/// rehearsed — a ref moved, a branch appeared, the worktree is dirty, or the
/// checkout changed. [`Error::Git`] or [`Error::Io`] if the transplant itself
/// fails.
pub fn run(sandbox: &Sandbox, now_unix: u64) -> Result<Applied> {
    let meta = sandbox.meta();
    let repo = meta.repo_path.as_path();
    let worktree = sandbox.worktree();

    let rehearsed = state_of(&worktree)?;
    let moved = ref_moves(&meta.pre_state, &rehearsed);
    if moved.is_empty() {
        return Err(Error::Refused(format!(
            "rehearsal {} moved no refs, so there is nothing to apply.\n\
             Discard it with `git rehearse discard {}`.",
            meta.id, meta.id
        )));
    }

    let now = state_of(repo)?;
    // Checkout first: switching branches also moves HEAD, and "you rehearsed
    // on main and are now on feature" is a better answer than "HEAD is now a
    // different commit".
    check_checkout(repo, &meta.checkout)?;
    check_unchanged(&meta.pre_state, &now, &meta.id)?;
    let reset = branch_to_reset(&meta.checkout, &moved);
    if reset.is_some() {
        check_clean(repo)?;
    }

    // Objects first: this adds nothing a user can see and leaves the
    // repository unchanged if it fails.
    let anchor = format!("refs/rehearse/{}/", meta.id);
    fetch_objects(repo, &worktree, &anchor)?;

    let undo = undo::write(repo, &Record::of_apply(meta.id.clone(), now_unix, &moved))?;

    transplant(repo, &moved, &meta.id)?;

    if reset.is_some() {
        // After the refs move, HEAD's branch points at the rehearsed commit
        // while the index and worktree still hold the old one. Resetting to
        // HEAD — not to a commit id — because HEAD is already right.
        git::run(repo, ["reset", "--hard", "--quiet"])?;
    }

    Ok(Applied {
        moved,
        reset,
        undo,
        anchor,
    })
}

/// A repository's branches and `HEAD`, in the same shape as the pre-state, so
/// the sandbox and the real repository can be compared directly.
fn state_of(repo: &Path) -> Result<BTreeMap<String, String>> {
    let mut refs = git::refs(repo, "refs/heads/", 0)?;
    if let Ok(head) = git::run(repo, ["rev-parse", "--verify", "--quiet", "HEAD"]) {
        refs.insert(HEAD_KEY.to_owned(), head);
    }
    Ok(refs)
}

/// Refuses if the repository is not where the rehearsal left it.
///
/// Every ref recorded in the pre-state has to still be there, at the same
/// commit — someone committing, pulling or switching branches between the
/// rehearsal and the apply means the report described a repository that no
/// longer exists.
fn check_unchanged(
    pre_state: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
    id: &str,
) -> Result<()> {
    let mut differences: Vec<String> = Vec::new();
    for (name, was) in pre_state {
        match now.get(name) {
            Some(is) if is == was => {}
            Some(is) => differences.push(format!("  {name} is now {is}, was {was}")),
            None => differences.push(format!("  {name} is gone, was {was}")),
        }
    }
    if differences.is_empty() {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "the repository has changed since rehearsal {id}:\n{}\n\
         The report you read described a repository that no longer exists, so applying it \
         could undo whatever happened in between. Rehearse again.",
        differences.join("\n")
    )))
}

/// Refuses if the user is somewhere else than when they rehearsed.
fn check_checkout(repo: &Path, checkout: &Checkout) -> Result<()> {
    let current = git::run(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    let matches = match (checkout, current.as_deref()) {
        (Checkout::Branch(rehearsed), Some(now)) => rehearsed == now,
        // A detached HEAD's commit is already covered by the pre-state check.
        (Checkout::Detached(_), None) => true,
        _ => false,
    };
    if matches {
        return Ok(());
    }
    let now = current.unwrap_or_else(|| "a detached HEAD".to_owned());
    let rehearsed = match checkout {
        Checkout::Branch(branch) => branch.clone(),
        Checkout::Detached(sha) => format!("a detached HEAD at {sha}"),
    };
    Err(Error::Refused(format!(
        "you rehearsed on {rehearsed} and are now on {now}.\n\
         Switch back, or rehearse again from here."
    )))
}

/// The branch whose worktree has to be reset, if any.
fn branch_to_reset(checkout: &Checkout, moved: &[RefMove]) -> Option<String> {
    let Checkout::Branch(branch) = checkout else {
        // A detached HEAD is not a ref the transplant moves: the commit it
        // points at stays valid whatever happens to the branches.
        return None;
    };
    let reference = format!("refs/heads/{branch}");
    moved
        .iter()
        .any(|moved| moved.name == reference)
        .then(|| branch.clone())
}

/// Refuses to reset a worktree that has work in it.
fn check_clean(repo: &Path) -> Result<()> {
    // Same rule as preflight: tracked changes only, because `git reset --hard`
    // discards those and leaves untracked files alone.
    let status = git::run(repo, ["status", "--porcelain", "--untracked-files=no"])?;
    if status.is_empty() {
        return Ok(());
    }
    Err(Error::Refused(
        "applying this rehearsal rewrites the branch you have checked out, and your worktree \
         has uncommitted changes that `git reset --hard` would destroy.\n\
         Commit or stash them first."
            .to_owned(),
    ))
}

/// Copies the rehearsed objects into the real repository.
///
/// Parked under `refs/rehearse/<id>/*` rather than fetched loose: an object
/// nothing points at is a candidate for garbage collection, and these are the
/// commits that are about to become the user's history. The namespace also
/// leaves a trail — after an apply, `git log refs/rehearse/<id>/<branch>` is
/// still the rehearsed result even if the branch moves on.
fn fetch_objects(repo: &Path, worktree: &Path, anchor: &str) -> Result<()> {
    let refspec = format!("+refs/heads/*:{anchor}*");
    git::run(
        repo,
        [
            std::ffi::OsString::from("fetch"),
            std::ffi::OsString::from("--no-tags"),
            std::ffi::OsString::from("--quiet"),
            worktree.as_os_str().to_os_string(),
            std::ffi::OsString::from(refspec),
        ],
    )?;
    Ok(())
}

/// The transplant itself: one transaction, all or nothing.
///
/// Every update states the value it expects to replace, so git verifies the
/// race check a second time — atomically, inside the ref store, where no
/// window exists between checking and writing. The earlier check exists to
/// produce a message a human can act on; this one exists to be correct.
fn transplant(repo: &Path, moved: &[RefMove], id: &str) -> Result<()> {
    let mut commands = String::new();
    for moved in moved {
        // HEAD follows its branch; moving it directly would detach it.
        if moved.name == HEAD_KEY {
            continue;
        }
        match (&moved.before, &moved.after) {
            (Some(before), Some(after)) => {
                let _ = write!(commands, "update {}\0{after}\0{before}\0", moved.name);
            }
            // `create`, not `update` with an empty old value: in NUL-delimited
            // mode an empty old value means "do not check", so a branch that
            // somebody else created with the same name in the meantime would
            // be silently overwritten. `create` fails if the ref exists, which
            // is the whole point.
            (None, Some(after)) => {
                let _ = write!(commands, "create {}\0{after}\0", moved.name);
            }
            (Some(before), None) => {
                let _ = write!(commands, "delete {}\0{before}\0", moved.name);
            }
            (None, None) => {}
        }
    }
    if commands.is_empty() {
        return Ok(());
    }
    git::run_with_stdin(
        repo,
        [
            "update-ref",
            // The reflog is where someone looks when they want to know what
            // happened to their branch; it should say.
            "-m",
            &format!("git-rehearse apply {id}"),
            "--stdin",
            "-z",
        ],
        Some(&commands),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{branch_to_reset, check_unchanged};
    use crate::analyze::RefMove;
    use crate::sandbox::Checkout;
    use std::collections::BTreeMap;

    fn state(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, sha)| ((*name).to_owned(), (*sha).to_owned()))
            .collect()
    }

    fn moved(name: &str) -> RefMove {
        RefMove {
            name: name.to_owned(),
            before: Some("aaa".to_owned()),
            after: Some("bbb".to_owned()),
        }
    }

    #[test]
    fn an_unchanged_repository_passes() {
        let pre = state(&[("refs/heads/main", "aaa"), ("HEAD", "aaa")]);
        assert!(check_unchanged(&pre, &pre, "id").is_ok());
    }

    #[test]
    fn a_ref_that_moved_is_named_with_both_commits() {
        let pre = state(&[("refs/heads/main", "aaa")]);
        let now = state(&[("refs/heads/main", "ccc")]);

        let error = check_unchanged(&pre, &now, "1786248000-00").expect_err("refused");

        let message = error.to_string();
        assert!(
            message.contains("refs/heads/main is now ccc, was aaa"),
            "{message}"
        );
        assert!(message.contains("Rehearse again"), "{message}");
    }

    #[test]
    fn a_ref_that_disappeared_is_also_a_change() {
        let pre = state(&[("refs/heads/gone", "aaa")]);
        let now = state(&[]);

        let error = check_unchanged(&pre, &now, "id").expect_err("refused");

        assert!(error.to_string().contains("is gone"), "{error}");
    }

    #[test]
    fn a_new_branch_appearing_is_not_by_itself_a_reason_to_refuse() {
        // It is not in the pre-state, so nothing about it can be clobbered by
        // a ref this rehearsal moves — and the transaction refuses anyway if
        // the rehearsal happens to create the same name.
        let pre = state(&[("refs/heads/main", "aaa")]);
        let now = state(&[("refs/heads/main", "aaa"), ("refs/heads/new", "ddd")]);
        assert!(check_unchanged(&pre, &now, "id").is_ok());
    }

    #[test]
    fn the_worktree_is_reset_only_when_the_checked_out_branch_is_rewritten() {
        assert_eq!(
            branch_to_reset(
                &Checkout::Branch("feature".to_owned()),
                &[moved("refs/heads/feature")]
            ),
            Some("feature".to_owned())
        );
        assert_eq!(
            branch_to_reset(
                &Checkout::Branch("feature".to_owned()),
                &[moved("refs/heads/other")]
            ),
            None,
            "another branch moving leaves this worktree alone"
        );
        assert_eq!(
            branch_to_reset(
                &Checkout::Detached("aaa".to_owned()),
                &[moved("refs/heads/feature")]
            ),
            None,
            "a detached HEAD keeps pointing at a commit that is still valid"
        );
    }
}
