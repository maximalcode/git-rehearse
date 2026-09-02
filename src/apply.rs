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
//!
//! Uncommitted work carried through the rehearsal is transplanted the same
//! way and for the same reason: [`crate::carry::restore`] checks out the tree
//! the sandbox produced rather than merging the user's changes here for the
//! first time. Principle 2 covers the worktree, not only the refs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::analyze::{RefMove, ref_moves};
use crate::carry::Carry;
use crate::preflight::HEAD_KEY;
use crate::sandbox::{Checkout, Sandbox};
use crate::undo::{self, Record};
use crate::{Error, Result, carry, git};

/// What an apply did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The refs that were moved, created or deleted.
    pub moved: Vec<RefMove>,
    /// The branch whose worktree was reset, if the rewritten branch was the
    /// one checked out.
    pub reset: Option<String>,
    /// The uncommitted paths that were put back on top of the reset worktree,
    /// if this rehearsal carried any and there was anything left to put back.
    pub carried: Option<Vec<String>>,
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
    // Only the reset path cares about the worktree at all: if the checked-out
    // branch was not rewritten, nothing below touches a single file, and what
    // is in the worktree is no more this apply's business than the weather.
    let carried = if let Some(branch) = reset.as_ref() {
        check_worktree(repo, &worktree, branch, meta.carry.as_ref(), &meta.id)?
    } else {
        None
    };

    // Objects first: this adds nothing a user can see and leaves the
    // repository unchanged if it fails.
    let anchor = format!("refs/rehearse/{}/", meta.id);
    fetch_objects(repo, &worktree, &anchor, &meta.id, carried.is_some())?;

    let undo = undo::write(repo, &Record::of_apply(meta.id.clone(), now_unix, &moved))?;

    transplant(repo, &moved, &meta.id)?;

    if reset.is_some() {
        // After the refs move, HEAD's branch points at the rehearsed commit
        // while the index and worktree still hold the old one. Resetting to
        // HEAD — not to a commit id — because HEAD is already right.
        git::run(repo, ["reset", "--hard", "--quiet"])?;
        if let Some(carried) = &carried {
            carry::restore(repo, carried.result)?;
        }
    }

    Ok(Applied {
        moved,
        reset,
        undo,
        anchor,
        carried: carried.map(|carried| carried.paths.to_vec()),
    })
}

/// The uncommitted work this apply has to put back over the reset worktree.
struct Carried<'a> {
    /// The commit the sandbox produced by replaying it — the tree that gets
    /// checked out, never merged. See [`crate::carry`].
    result: &'a str,
    /// The paths it restores, for the report.
    paths: &'a [String],
}

/// Decides what may happen to the worktree, and refuses if the answer is
/// nothing good.
///
/// Two rules, and they are the same rule seen from two sides. A rehearsal that
/// promised nothing about this worktree needs it clean, because
/// `git reset --hard` would destroy whatever is there and it was never in the
/// report. A rehearsal that promised to put work back needs the worktree to
/// still hold *exactly* that work, for the same reason: what the report
/// promised was rehearsed, and anything edited since was not.
///
/// "Promised" rather than "carried": a rehearsal whose replay conflicted, or
/// never ran because nothing was going to move this worktree, carried the work
/// but made no claim about putting it back — so it falls under the first rule.
fn check_worktree<'a>(
    repo: &Path,
    rehearsed: &Path,
    branch: &str,
    carry: Option<&'a Carry>,
    id: &str,
) -> Result<Option<Carried<'a>>> {
    let Some(carry) = carry.filter(|carry| carry.promises_the_worktree()) else {
        check_clean(repo, carry.is_some())?;
        check_untracked(repo, rehearsed, branch)?;
        return Ok(None);
    };
    carry::check_unchanged(repo, carry, id)?;
    check_untracked(repo, rehearsed, branch)?;
    Ok(carry::result_of(carry).map(|result| Carried {
        result,
        paths: &carry.paths,
    }))
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
///
/// Only for a rehearsal that carried nothing — which means the worktree was
/// clean when it was rehearsed, so anything in it now appeared afterwards and
/// was never in the report. Carried work goes through
/// [`carry::check_unchanged`] instead.
fn check_clean(repo: &Path, carried: bool) -> Result<()> {
    // Tracked changes only, because `git reset --hard` discards those and
    // leaves untracked files alone.
    let status = git::run(repo, ["status", "--porcelain", "--untracked-files=no"])?;
    if status.is_empty() {
        return Ok(());
    }
    // Two ways to arrive here and they need different advice, because one of
    // them has a rehearsal sitting there that could still be finished.
    let why = if carried {
        "The rehearsal carried them, but they never went back on in the sandbox, so nothing \
         in the report accounts for them — carry it on with `git rehearse continue`, or \
         commit or stash them."
    } else {
        "They were not there when you rehearsed, so nothing in the report accounts for them — \
         commit or stash them, or rehearse again from here."
    };
    Err(Error::Refused(format!(
        "applying this rehearsal rewrites the branch you have checked out, and your worktree \
         has uncommitted changes that `git reset --hard` would destroy.\n{why}"
    )))
}

/// Refuses to reset a worktree over an untracked path that the rehearsed
/// checkout would need to write.
///
/// Git status deliberately leaves untracked files out of the ordinary clean
/// check above, because a hard reset normally leaves them alone. A reset does
/// replace one, however, when the target tree tracks the same path (or when a
/// file and directory exchange means one path contains the other). Compare
/// every other file — including ignored files — with the target branch's tree
/// before fetching anything or moving any refs.
fn check_untracked(repo: &Path, rehearsed: &Path, branch: &str) -> Result<()> {
    let untracked_output = git::run(repo, ["ls-files", "--others", "--no-empty-directory", "-z"])?;
    let untracked = untracked_output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if untracked.is_empty() {
        return Ok(());
    }

    let target = format!("refs/heads/{branch}");
    let tracked_output = git::run(rehearsed, ["ls-tree", "-r", "--name-only", "-z", &target])?;
    let tracked = tracked_output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let collisions = untracked
        .iter()
        .filter(|untracked| {
            tracked.iter().any(|tracked| {
                untracked.as_str() == tracked.as_str()
                    || tracked
                        .as_str()
                        .strip_prefix(untracked.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
                    || untracked
                        .as_str()
                        .strip_prefix(tracked.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if collisions.is_empty() {
        return Ok(());
    }

    Err(Error::Refused(format!(
        "applying this rehearsal would overwrite untracked file(s):\n  {}\n\
         Keep them elsewhere or rehearse again from here.",
        collisions.join("\n  ")
    )))
}

/// Copies the rehearsed objects into the real repository.
///
/// Parked under `refs/rehearse/<id>/*` rather than fetched loose: an object
/// nothing points at is a candidate for garbage collection, and these are the
/// commits that are about to become the user's history. The namespace also
/// leaves a trail — after an apply, `git log refs/rehearse/<id>/<branch>` is
/// still the rehearsed result even if the branch moves on.
fn fetch_objects(
    repo: &Path,
    worktree: &Path,
    anchor: &str,
    id: &str,
    carried: bool,
) -> Result<()> {
    let mut args = vec![
        std::ffi::OsString::from("fetch"),
        std::ffi::OsString::from("--no-tags"),
        std::ffi::OsString::from("--quiet"),
        worktree.as_os_str().to_os_string(),
        std::ffi::OsString::from(format!("+refs/heads/*:{anchor}*")),
    ];
    // The replayed worktree comes with them, anchored the same way: it is a
    // commit the report described and apply is about to check out, so it must
    // not be collectable in between.
    if carried {
        args.push(std::ffi::OsString::from(carry::result_refspec(id)));
    }
    git::run(repo, args)?;
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
