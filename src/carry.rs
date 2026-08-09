//! Carrying uncommitted work through a rehearsal.
//!
//! SCOPE.md's v1.x item 2. Until this existed, a dirty worktree was a flat
//! refusal, which answered half of the question people actually have. Nobody
//! rehearsing a rebase wants to know only "does the rebase work"; they want to
//! know **"does the rebase work *and do I get my uncommitted work back*"**.
//! Answering the second half is what this module is for, and it answers it in
//! the sandbox, where being wrong costs nothing.
//!
//! # The four steps
//!
//! 1. **Snapshot** — [`snapshot`] takes `git stash create` in the real
//!    repository. That writes the commit object and prints its id, touching
//!    neither the worktree nor `refs/stash`: the user's stash list is theirs,
//!    and a rehearsal must never appear in it. The snapshot costs nothing if
//!    the rehearsal is thrown away — an unreferenced commit that `git gc`
//!    collects.
//! 2. **Transfer** — [`park`] pushes that one commit into the sandbox. A local
//!    clone copies what is *reachable*, and an unreferenced stash commit is
//!    not, so it has to be moved deliberately and parked under a ref so
//!    nothing in the sandbox collects it before it is used.
//! 3. **Rehearse clean** — nothing above puts the changes into the sandbox
//!    worktree. `git rebase` refuses to run on a dirty tree, and rehearsing
//!    something git would refuse to run is not a rehearsal, so the sandbox is
//!    clean for the command itself.
//! 4. **Replay, afterwards** — [`replay`] runs `git stash apply` in the
//!    sandbox once the command has finished, and captures the result as
//!    another stash commit. *That commit is the answer*, and it is also what
//!    apply transplants.
//!
//! # Why apply transplants the replay's result
//!
//! Design principle 2 — apply is a ref transplant, never a re-run — applies to
//! the carried work exactly as it applies to the commits. Restoring the
//! changes with a `git stash apply` in the real repository would be a merge
//! run for the first time in the one place that must not have first times: it
//! could conflict there having been reported as clean, and the report would
//! have described something that had not happened yet. So the replay happens
//! in the sandbox, the report states its result, and [`restore`] checks the
//! rehearsed tree out over the reset worktree. What was inspected is what
//! comes back.
//!
//! # A conflicting replay is a stopped rehearsal, and nothing new
//!
//! This is the decision the issue was filed for, so it is written down rather
//! than left in the code. When the command runs clean but the carried work
//! does not go back on cleanly, the state is: a sandbox holding the rehearsed
//! history, with conflict markers in its worktree and unmerged entries in its
//! index. That is *the same shape* as a rebase that stopped on a conflict —
//! a real repository, mid-operation, waiting for a person — and the tool
//! already has a vocabulary for it: exit `2`, keep the sandbox rather than
//! discard it, print where it is, and resolve there and
//! `git rehearse continue`. Inventing a fourth [`Outcome`] would duplicate all
//! of that to describe a difference the user does not experience.
//!
//! Two consequences follow, both wanted. Applying is not offered while the
//! replay is unresolved, because half an answer is not a result anybody
//! inspected — the refs are fine, but the report's promise about the
//! uncommitted work is unkept, and the real worktree still holds that work
//! untouched, so nothing is lost by declining. And `continue` finishes the
//! replay instead of the command: [`resume`] captures what the user resolved
//! rather than re-running `git stash apply` over it, because a resolution
//! replayed again is a resolution thrown away.
//!
//! # What stays out
//!
//! Untracked files. They are not in a stash without `-u`, git does not destroy
//! them, and widening the snapshot widens what apply has to put back. The
//! tracked-changes-only boundary is the same one the refusal drew, and it
//! stays where it was.
//!
//! # One thing this is not
//!
//! For a command whose *point* is to destroy uncommitted work —
//! `git reset --hard` through the `--` hatch — carrying it makes the rehearsal
//! kinder than the command: the work would come back, where the real thing
//! would have eaten it. Nothing about that is silent, because the report says
//! on its own line what was carried and that it comes back. But it is why
//! `reset --hard` stays off the agent intercept list in SCOPE's v2 section: an
//! intercept that quietly turns a destructive command into a safe one is a
//! different product from a rehearsal.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::execute::{self, Outcome};
use crate::sandbox::{Checkout, Meta, Sandbox};
use crate::{Error, Result, git};

/// Where the sandbox parks the snapshot pushed in from the real repository.
const SNAPSHOT_REF: &str = "refs/rehearse/carried";

/// Where the sandbox parks the replay's result, so that nothing collects it
/// between the rehearsal and an apply days later.
const RESULT_REF: &str = "refs/rehearse/replayed";

/// How many paths a summary names before it says "and N more".
const PATHS_NAMED: usize = 3;

/// The uncommitted work a rehearsal carries, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carry {
    /// The `git stash create` commit taken in the real repository.
    ///
    /// The same object id in the sandbox, because it was pushed there rather
    /// than rebuilt — which is what makes it the *same* changes and not an
    /// equivalent set.
    pub snapshot: String,
    /// The tracked paths that were dirty, as `git status` named them. For the
    /// report; the snapshot is the evidence.
    pub paths: Vec<String>,
    /// What happened when the changes were put back in the sandbox. `None`
    /// until the rehearsed command has finished — a command that stopped or
    /// failed leaves nothing to put them on.
    pub replay: Option<Replay>,
}

/// The result of replaying the carried work in the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Replay {
    /// The changes went back on cleanly.
    Restored {
        /// The commit capturing the sandbox worktree afterwards — the tree
        /// apply checks out. Absent when there was nothing left to capture,
        /// which means the rehearsed history already contains the changes.
        result: Option<String>,
    },
    /// The changes conflict with the rehearsed history. See the module docs:
    /// this is a stopped rehearsal, not a new kind of state.
    Conflicted {
        /// The unmerged paths in the sandbox.
        paths: Vec<String>,
    },
    /// The replay could not be attempted at all, in git's own words.
    Refused {
        /// Why, as git said it.
        reason: String,
    },
    /// There was nothing to replay onto: this rehearsal does not move the
    /// user's worktree, so the changes simply stay where they are.
    ///
    /// Rehearsing a branch you are not standing on, or one the command left
    /// alone, or a detached `HEAD` — apply resets nothing in any of those, so
    /// replaying would answer a question nobody asked, against whatever branch
    /// the command happened to leave the sandbox on.
    NotNeeded,
}

impl Replay {
    /// The rehearsal outcome this replay implies.
    ///
    /// The command had already finished cleanly to get here, so this is the
    /// whole rehearsal's outcome: a rehearsal that cannot put your work back
    /// has not finished, whatever the rebase did.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        match self {
            Self::Restored { .. } | Self::NotNeeded => Outcome::Clean,
            Self::Conflicted { .. } => Outcome::Stopped { conflicts: true },
            // Stopped, but nothing is unmerged: calling that a conflict would
            // send someone looking for markers that are not there.
            Self::Refused { .. } => Outcome::Stopped { conflicts: false },
        }
    }

    /// Whether the rehearsal is waiting on this replay rather than on the
    /// command — the case `continue` finishes rather than resumes.
    #[must_use]
    pub fn is_unfinished(&self) -> bool {
        matches!(self, Self::Conflicted { .. } | Self::Refused { .. })
    }
}

impl Carry {
    /// Whether the report promised anything about the worktree apply is about
    /// to reset.
    ///
    /// Only a replay that ran and came back clean does. Everything else means
    /// the uncommitted work in the real repository was never rehearsed onto
    /// the new history, so apply must treat that worktree exactly as it treats
    /// a rehearsal that carried nothing: refuse to reset over it.
    #[must_use]
    pub fn promises_the_worktree(&self) -> bool {
        matches!(self.replay, Some(Replay::Restored { .. }))
    }
}

/// Whether this rehearsal stopped on the replay rather than on the command.
///
/// The distinction is invisible in [`Outcome`] on purpose (see the module
/// docs), so the report and `continue` read it off the record instead.
#[must_use]
pub fn stopped_on_replay(meta: &Meta) -> bool {
    meta.carry
        .as_ref()
        .and_then(|carry| carry.replay.as_ref())
        .is_some_and(Replay::is_unfinished)
}

/// Snapshots the uncommitted work in `repo`, if there is any.
///
/// # Errors
///
/// [`Error::Refused`] if the worktree is dirty in a way git will not snapshot
/// — unmerged paths from an operation the user is in the middle of, most
/// likely. [`Error::Git`] if the repository cannot be read.
pub fn snapshot(repo: &Path) -> Result<Option<Carry>> {
    // Untracked files are deliberately excluded, here and in the stash: `git
    // rebase` runs happily with them present, and principle 5 refuses where
    // git refuses rather than being stricter than git.
    let status = git::run(repo, ["status", "--porcelain", "--untracked-files=no"])?;
    let paths = paths_of(&status);
    if paths.is_empty() {
        return Ok(None);
    }

    // `create`, never `push` or `save`: it writes the commit and prints its
    // id without touching the worktree or refs/stash. The user's stash list is
    // theirs.
    let snapshot = match git::run(repo, ["stash", "create"]) {
        Ok(sha) if !sha.is_empty() => sha,
        Ok(_) => {
            return Err(Error::Refused(format!(
                "your worktree has changes ({}) that git will not snapshot.\n\
                 Commit or stash them and rehearse again.",
                describe(&paths)
            )));
        }
        Err(Error::Git { stderr, .. }) => {
            return Err(Error::Refused(format!(
                "your uncommitted changes cannot be snapshotted: {}\n\
                 Finish or abort whatever is in progress, or commit them, and rehearse again.",
                first_line(&stderr)
            )));
        }
        Err(other) => return Err(other),
    };

    Ok(Some(Carry {
        snapshot,
        paths,
        replay: None,
    }))
}

/// Moves the snapshot into the sandbox and parks it under a ref.
///
/// A push from the real repository rather than a fetch from the sandbox, and
/// the reason is not taste: `git fetch` will not ask for a bare object id
/// unless the far side sets `uploadpack.allowAnySHA1InWant`, while a push
/// takes any SHA-1 expression as the source of a refspec and needs no
/// configuration on either side. Nothing is left behind by it — the sandbox
/// gains a ref, not a remote (principle 3), and the real repository gains
/// nothing at all.
///
/// `--no-verify` because a push runs the *source* repository's `pre-push`
/// hook, and a rehearsal must not fire the user's hooks. `--no-follow-tags` so
/// the transfer is exactly the one object graph we asked for.
///
/// # Errors
///
/// [`Error::Git`] if the transfer fails.
pub fn park(repo: &Path, worktree: &Path, carry: &Carry) -> Result<()> {
    git::run(
        repo,
        [
            OsString::from("push"),
            OsString::from("--quiet"),
            OsString::from("--no-verify"),
            OsString::from("--no-follow-tags"),
            worktree.as_os_str().to_os_string(),
            OsString::from(format!("{}:{SNAPSHOT_REF}", carry.snapshot)),
        ],
    )?;
    Ok(())
}

/// Puts the carried work back in the sandbox, after the command has run.
///
/// # Errors
///
/// [`Error::Git`] if the sandbox cannot be read.
pub fn replay(worktree: &Path) -> Result<Replay> {
    // `apply`, not `pop`: there is no refs/stash entry to drop, and the
    // snapshot ref is what apply's transplant is anchored to.
    match git::run(worktree, ["stash", "apply", "--quiet", SNAPSHOT_REF]) {
        Ok(_) => {}
        Err(Error::Git { stderr, .. }) => {
            let paths = execute::unmerged(worktree)?;
            return Ok(if paths.is_empty() {
                Replay::Refused {
                    reason: first_line(&stderr),
                }
            } else {
                Replay::Conflicted { paths }
            });
        }
        Err(other) => return Err(other),
    }
    Ok(Replay::Restored {
        result: capture(worktree)?,
    })
}

/// Records the replay, and turns the rehearsal's outcome into the one that
/// describes both halves of it.
///
/// A command that did not finish is returned untouched: there is no rehearsed
/// history to put anything on, and a rehearsal that stopped in the rebase is
/// already stopped.
///
/// # Errors
///
/// As [`replay`], plus [`Error::Io`] if `meta.json` cannot be rewritten.
pub fn after_command(sandbox: &mut Sandbox, outcome: Outcome) -> Result<Outcome> {
    if outcome != Outcome::Clean {
        return Ok(outcome);
    }
    let Some(carry) = sandbox.meta().carry.clone() else {
        return Ok(outcome);
    };
    // Already replayed: `continue` on a finished rehearsal, which resume()
    // handles rather than this.
    if carry.replay.is_some() {
        return Ok(outcome);
    }

    let worktree = sandbox.worktree();
    let replay = if moves_the_worktree(&worktree, sandbox.meta()) {
        replay(&worktree)?
    } else {
        Replay::NotNeeded
    };
    let outcome = replay.outcome();
    sandbox.record_replay(replay)?;
    Ok(outcome)
}

/// Whether applying this rehearsal would reset the worktree the carried work
/// is sitting in.
///
/// Three things have to hold, and each one is a way the replay would otherwise
/// be answering the wrong question. The rehearsal has to have started on a
/// branch, because apply never resets a detached `HEAD`. The sandbox has to
/// still be on that branch, because a command such as `git rebase main topic`
/// leaves it standing somewhere else entirely and the worktree there is not
/// the user's. And that branch has to have moved, because a branch that did
/// not move takes nobody's worktree with it.
/// A failed read answers "no", deliberately: the fallback is to replay
/// nothing, leave the user's worktree alone and let apply refuse rather than
/// reset over work nothing has accounted for.
fn moves_the_worktree(worktree: &Path, meta: &Meta) -> bool {
    let Checkout::Branch(branch) = &meta.checkout else {
        return false;
    };
    let on = git::run(worktree, ["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    if on.as_deref() != Some(branch.as_str()) {
        return false;
    }
    let name = format!("refs/heads/{branch}");
    let now = git::run(worktree, ["rev-parse", "--verify", "--quiet", &name]).ok();
    meta.pre_state.get(&name) != now.as_ref()
}

/// Carries on a rehearsal that stopped on its replay.
///
/// Two shapes, and they need opposite treatment. A [`Replay::Conflicted`] has
/// already been merged into the sandbox worktree and then resolved there by
/// the user, so this **captures what is there** — re-running `git stash apply`
/// over a resolution would throw the resolution away, which is the same
/// mistake as re-running a rehearsed command. A [`Replay::Refused`] never got
/// as far as merging anything, so this tries again.
///
/// # Errors
///
/// [`Error::Refused`] if paths are still unmerged, or if there is nothing
/// waiting. [`Error::Git`] if the sandbox cannot be read.
pub fn resume(sandbox: &mut Sandbox) -> Result<Outcome> {
    let worktree = sandbox.worktree();
    let waiting = sandbox
        .meta()
        .carry
        .as_ref()
        .and_then(|carry| carry.replay.as_ref())
        .filter(|replay| replay.is_unfinished())
        .cloned();

    let replay = match waiting {
        Some(Replay::Conflicted { .. }) => {
            // Refused before anything is captured, and with the same words
            // `continue` uses for a stopped command: a stash commit made over
            // unmerged entries would bake the conflict markers in.
            let unmerged = execute::unmerged(&worktree)?;
            if !unmerged.is_empty() {
                return Err(Error::Refused(format!(
                    "{} path(s) are still unmerged in the sandbox:\n  {}\n\
                     Resolve them and `git add` them there, then continue.",
                    unmerged.len(),
                    unmerged.join("\n  ")
                )));
            }
            Replay::Restored {
                result: capture(&worktree)?,
            }
        }
        Some(Replay::Refused { .. }) => replay(&worktree)?,
        Some(Replay::Restored { .. } | Replay::NotNeeded) | None => {
            return Err(Error::Refused(
                "this rehearsal has nothing in progress — there is nothing to continue.\n\
                 `git rehearse show` prints the report again; `apply` transplants it."
                    .to_owned(),
            ));
        }
    };

    let outcome = replay.outcome();
    sandbox.record_replay(replay)?;
    Ok(outcome)
}

/// Captures the sandbox worktree as a commit, parked so nothing collects it.
///
/// `None` means there was nothing to capture: the carried changes turned out
/// to be contained in the rehearsed history already, so replaying them was a
/// no-op and there is nothing for apply to put back.
fn capture(worktree: &Path) -> Result<Option<String>> {
    let sha = git::run(worktree, ["stash", "create"])?;
    if sha.is_empty() {
        return Ok(None);
    }
    git::run(worktree, ["update-ref", RESULT_REF, &sha])?;
    Ok(Some(sha))
}

/// The refspec that brings the replay's result into the real repository.
///
/// A sibling of the rehearsal's own namespace rather than a child of it: a
/// branch called `carry` would otherwise collide with it, and a fetch that
/// fails on a legal branch name is a worse trade than a longer ref.
#[must_use]
pub fn result_refspec(id: &str) -> String {
    format!("+{RESULT_REF}:refs/rehearse/{id}-carry")
}

/// The commit apply has to check out, if this rehearsal has one.
#[must_use]
pub fn result_of(carry: &Carry) -> Option<&str> {
    match &carry.replay {
        Some(Replay::Restored { result }) => result.as_deref(),
        Some(Replay::Conflicted { .. } | Replay::Refused { .. } | Replay::NotNeeded) | None => None,
    }
}

/// Refuses unless the worktree still holds exactly the changes that were
/// carried.
///
/// The report described putting *those* changes back on the rehearsed history.
/// Anything edited since was never rehearsed, never in the report, and would
/// be silently destroyed by the reset — so it is refused, which is the same
/// rule the ref race check follows and the same one the flat dirty-worktree
/// refusal followed before this feature existed.
///
/// Compared by tree rather than by commit id, because `git stash create` stamps
/// a commit time and so never produces the same id twice. Both trees are
/// compared — the worktree's and the index's — so that restaging counts as a
/// change too, since the restore cannot preserve what was staged.
///
/// # Errors
///
/// [`Error::Refused`] if the changes are gone or are not the ones that were
/// carried. [`Error::Git`] if the repository cannot be read.
pub fn check_unchanged(repo: &Path, carry: &Carry, id: &str) -> Result<()> {
    let now = git::run(repo, ["stash", "create"])?;
    if now.is_empty() {
        return Err(Error::Refused(format!(
            "the uncommitted changes rehearsal {id} carried are no longer in your worktree.\n\
             Applying would put them back — rehearse again from where you are now."
        )));
    }
    if trees_of(repo, &now)? != trees_of(repo, &carry.snapshot)? {
        return Err(Error::Refused(format!(
            "your uncommitted changes are not the ones rehearsal {id} carried.\n\
             What the report promised to put back was rehearsed; what is in your worktree now \
             was not. Rehearse again."
        )));
    }
    Ok(())
}

/// A stash commit's two trees: the worktree it captured, and the index.
fn trees_of(repo: &Path, commit: &str) -> Result<(String, Option<String>)> {
    let worktree = git::run(repo, ["rev-parse", &format!("{commit}^{{tree}}")])?;
    // The second parent is the index state. Optional rather than required so a
    // stash-shaped commit without one compares on its worktree alone instead
    // of turning an apply into an internal error.
    let index = git::run(repo, ["rev-parse", &format!("{commit}^2^{{tree}}")]).ok();
    Ok((worktree, index))
}

/// Puts the rehearsed result into the worktree, after the reset.
///
/// Not a merge, and not a `git stash apply`: `result` is a tree that was
/// produced in the sandbox and inspected in the report, and this checks it
/// out. There is nothing here that can conflict, which is the entire point.
///
/// The index is then put back to `HEAD`, so the changes come back as worktree
/// modifications — the same place `git stash pop` without `--index` leaves
/// them. What was staged before the rehearsal is not restored as staged; a
/// transplanted tree does not carry that distinction, and inventing one would
/// mean guessing.
///
/// # Errors
///
/// [`Error::Git`] if the checkout fails.
pub fn restore(repo: &Path, result: &str) -> Result<()> {
    git::run(
        repo,
        ["read-tree", "-u", "--reset", &format!("{result}^{{tree}}")],
    )?;
    git::run(repo, ["reset", "--mixed", "--quiet", "HEAD"])?;
    Ok(())
}

/// The paths in `git status --porcelain` output.
#[must_use]
pub fn paths_of(status: &str) -> Vec<String> {
    status
        .lines()
        // "XY path" — the two status columns and their space are always ASCII.
        .filter_map(|line| line.get(3..))
        .map(str::to_owned)
        .collect()
}

/// Names a few paths and counts the rest.
///
/// A report line is read at a glance; twelve file names in it are not read at
/// all.
#[must_use]
pub fn describe(paths: &[String]) -> String {
    let mut named = paths
        .iter()
        .take(PATHS_NAMED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(rest) = paths.len().checked_sub(PATHS_NAMED).filter(|n| *n > 0) {
        let _ = write!(named, " and {rest} more");
    }
    named
}

/// Git's first line of complaint, which is the one worth repeating.
fn first_line(stderr: &str) -> String {
    stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("git said nothing")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{Carry, Replay, describe, first_line, paths_of, result_of, result_refspec};
    use crate::execute::Outcome;

    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn carry(replay: Option<Replay>) -> Carry {
        Carry {
            snapshot: "aaaa1111".to_owned(),
            paths: paths(&["src/main.rs"]),
            replay,
        }
    }

    #[test]
    fn status_output_becomes_the_paths_that_were_carried() {
        assert_eq!(
            paths_of(" M src/main.rs\nM  Cargo.toml\nD  gone.rs"),
            paths(&["src/main.rs", "Cargo.toml", "gone.rs"])
        );
        assert!(paths_of("").is_empty());
    }

    #[test]
    fn a_long_list_of_paths_is_summarised_rather_than_dumped() {
        let many: Vec<String> = (0..12).map(|n| format!("file{n}.rs")).collect();
        let summary = describe(&many);
        assert!(
            summary.starts_with("file0.rs, file1.rs, file2.rs"),
            "{summary}"
        );
        assert!(summary.ends_with("and 9 more"), "{summary}");
        assert!(!summary.contains("file11.rs"), "{summary}");
    }

    #[test]
    fn a_short_list_is_named_in_full_without_a_count() {
        assert_eq!(describe(&paths(&["a.rs", "b.rs"])), "a.rs, b.rs");
    }

    #[test]
    fn a_replay_that_conflicts_makes_the_whole_rehearsal_a_stopped_one() {
        // The decision this module exists to record: a conflicting replay is
        // the state `continue` already knows how to work with, not a new one.
        assert_eq!(
            Replay::Conflicted {
                paths: paths(&["notes.txt"])
            }
            .outcome(),
            Outcome::Stopped { conflicts: true }
        );
        assert_eq!(
            Replay::Refused {
                reason: "dirty".to_owned()
            }
            .outcome(),
            Outcome::Stopped { conflicts: false },
            "nothing is unmerged, so calling it a conflict would be a lie"
        );
        assert_eq!(
            Replay::Restored {
                result: Some("bbbb".to_owned())
            }
            .outcome(),
            Outcome::Clean
        );
    }

    #[test]
    fn only_an_unfinished_replay_is_something_continue_takes_over() {
        assert!(Replay::Conflicted { paths: Vec::new() }.is_unfinished());
        assert!(
            Replay::Refused {
                reason: String::new()
            }
            .is_unfinished()
        );
        assert!(!Replay::Restored { result: None }.is_unfinished());
    }

    #[test]
    fn only_a_restored_replay_offers_apply_something_to_check_out() {
        assert_eq!(
            result_of(&carry(Some(Replay::Restored {
                result: Some("cccc".to_owned())
            }))),
            Some("cccc")
        );
        // Nothing came back because the rehearsed history already contains it:
        // the worktree after the reset is already right.
        assert_eq!(
            result_of(&carry(Some(Replay::Restored { result: None }))),
            None
        );
        assert_eq!(
            result_of(&carry(Some(Replay::Conflicted { paths: Vec::new() }))),
            None
        );
        assert_eq!(result_of(&carry(None)), None);
    }

    #[test]
    fn the_result_lands_beside_the_rehearsals_namespace_not_inside_it() {
        // Inside it, a branch called `carry` would make the fetch fail with a
        // directory/file conflict on a perfectly legal branch name.
        assert_eq!(
            result_refspec("1786248000-00"),
            "+refs/rehearse/replayed:refs/rehearse/1786248000-00-carry"
        );
    }

    #[test]
    fn gits_first_words_are_the_ones_repeated_back() {
        assert_eq!(
            first_line("\nerror: your local changes would be overwritten\nhint: stash them\n"),
            "error: your local changes would be overwritten"
        );
        assert_eq!(first_line("   "), "git said nothing");
    }
}
