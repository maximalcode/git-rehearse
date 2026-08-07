//! What the rehearsal actually did — read out of the sandbox, not inferred
//! from the command.
//!
//! Three questions, in the order a person asks them:
//!
//! 1. **What moved?** Every branch and `HEAD` whose commit differs from the
//!    pre-state snapshot, old → new.
//! 2. **Where did it stop?** For a stopped command: which commit was being
//!    replayed, which files are unmerged, and how many conflict hunks each has.
//! 3. **Did the content change?** For an operation that is supposed to
//!    preserve content, the old tip's tree against the new tip's tree. A
//!    difference here is the silent semantic change that justifies the whole
//!    tool.
//!
//! Everything is read by running git. gix is named in SCOPE.md as an option
//! for the read side, and the note there is explicit that it is an
//! optimisation rather than a principle: every read below is one short git
//! invocation against a local repository, so a second object-database
//! implementation would add a large dependency tree, a second set of
//! behaviours to keep in step with the user's git, and nothing a user can
//! perceive. Revisit if profiling ever says otherwise.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::execute::Outcome;
use crate::preflight::HEAD_KEY;
use crate::{Result, git};

/// Everything the report needs to know about a rehearsal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// Refs whose commit changed, sorted by name.
    pub ref_moves: Vec<RefMove>,
    /// The commit the command stopped on, if it stopped.
    pub stopped_at: Option<Commit>,
    /// Unmerged paths and their conflict-hunk counts.
    pub conflicts: Vec<Conflict>,
    /// Content differences between the old and new tip of each rewritten
    /// branch.
    pub drift: Vec<Drift>,
    /// Whether [`Analysis::drift`] being non-empty is a surprise.
    ///
    /// True for `rebase` and `cherry-pick`, which replay the same changes onto
    /// a different parent and should therefore land on the same content. False
    /// for `merge`, where changed content is the entire point — the report
    /// says "content differs" either way, but only one of them is a warning.
    pub drift_expected_empty: bool,
}

impl Analysis {
    /// Whether this rehearsal changed content it was not supposed to change.
    #[must_use]
    pub fn has_unexpected_drift(&self) -> bool {
        self.drift_expected_empty && !self.drift.is_empty()
    }
}

/// One ref, before and after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMove {
    /// Full ref name, or [`HEAD_KEY`] for `HEAD` itself.
    pub name: String,
    /// Where it pointed before the command; `None` if the command created it.
    pub before: Option<String>,
    /// Where it points now; `None` if the command deleted it.
    pub after: Option<String>,
}

/// A commit, identified and described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Full object name.
    pub sha: String,
    /// First line of the commit message.
    pub subject: String,
}

/// An unmerged path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Path, relative to the worktree root.
    pub path: String,
    /// Number of conflict hunks git left in the file.
    ///
    /// Zero means the file is unmerged but carries no markers — a binary file,
    /// or an add/add or delete/modify conflict. Worth showing as-is rather
    /// than rounding up to one: the two need different handling.
    pub hunks: usize,
}

/// The content difference between a branch's old and new tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// The ref that was rewritten.
    pub reference: String,
    /// Files whose content differs between the two tips.
    pub changes: Vec<FileChange>,
    /// Commits between the common ancestor and the old tip.
    pub commits_before: usize,
    /// Commits between the common ancestor and the new tip.
    ///
    /// More than `commits_before` usually means the rewrite picked up commits
    /// from a base that had moved on, which explains a content difference that
    /// would otherwise look alarming.
    pub commits_after: usize,
}

/// One file in a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Git's status letter: `M`, `A`, `D`, `R`…
    pub status: String,
    /// Path, relative to the worktree root.
    pub path: String,
}

/// Reads the sandbox and works out what the command did to it.
///
/// `pre_state` is preflight's snapshot — the refs as they were before the
/// clone, which is also what the sandbox started from.
///
/// # Errors
///
/// [`Error::Git`](crate::Error::Git) if a read fails, [`Error::Io`](crate::Error::Io)
/// if a conflicted file cannot be examined.
pub fn run(
    worktree: &Path,
    pre_state: &BTreeMap<String, String>,
    command: &[String],
    outcome: &Outcome,
) -> Result<Analysis> {
    let after = current_state(worktree)?;
    let ref_moves = ref_moves(pre_state, &after);

    let stopped = matches!(outcome, Outcome::Stopped { .. });
    let (stopped_at, conflicts) = if stopped {
        (stopped_at(worktree)?, conflicts(worktree)?)
    } else {
        (None, Vec::new())
    };

    Ok(Analysis {
        drift: drift(worktree, &ref_moves)?,
        drift_expected_empty: preserves_content(command),
        ref_moves,
        stopped_at,
        conflicts,
    })
}

/// The sandbox's refs right now, in the same shape as the pre-state.
fn current_state(worktree: &Path) -> Result<BTreeMap<String, String>> {
    let mut refs = git::refs(worktree, "refs/heads/", 0)?;
    // An unborn HEAD (every branch deleted) simply has no entry, which the
    // comparison reads as a deletion.
    if let Ok(head) = git::run(worktree, ["rev-parse", "--verify", "--quiet", "HEAD"]) {
        refs.insert(HEAD_KEY.to_owned(), head);
    }
    Ok(refs)
}

/// Every ref whose commit differs between the two states.
fn ref_moves(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<RefMove> {
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    names
        .into_iter()
        .filter(|name| before.get(*name) != after.get(*name))
        .map(|name| RefMove {
            name: name.clone(),
            before: before.get(name).cloned(),
            after: after.get(name).cloned(),
        })
        .collect()
}

/// The commit the command stopped on.
fn stopped_at(worktree: &Path) -> Result<Option<Commit>> {
    // Asked for rather than assumed to be `<worktree>/.git`: if git cannot say
    // where its own directory is, guessing at one and reporting "stopped
    // nowhere" would be worse than the error.
    let git_dir = PathBuf::from(git::run(worktree, ["rev-parse", "--absolute-git-dir"])?);

    // A rebase records the commit it was replaying; the other sequencer
    // operations record the commit being applied as a ref.
    let from_file = ["rebase-merge/stopped-sha", "rebase-apply/original-commit"]
        .iter()
        .find_map(|name| fs::read_to_string(git_dir.join(name)).ok())
        .map(|sha| sha.trim().to_owned());

    let candidate = match from_file {
        Some(sha) => Some(sha),
        None => ["CHERRY_PICK_HEAD", "REVERT_HEAD", "MERGE_HEAD"]
            .iter()
            .find_map(|name| git::run(worktree, ["rev-parse", "--verify", "--quiet", name]).ok()),
    };

    let Some(candidate) = candidate else {
        return Ok(None);
    };
    // stopped-sha is abbreviated, and a ref name is not an object name, so
    // both go back through git to become a full commit.
    let Ok(sha) = git::run(
        worktree,
        ["rev-parse", "--verify", &format!("{candidate}^{{commit}}")],
    ) else {
        return Ok(None);
    };
    let subject = git::run(worktree, ["log", "-1", "--format=%s", &sha])?;
    Ok(Some(Commit { sha, subject }))
}

/// Unmerged paths, with the number of conflict hunks in each.
fn conflicts(worktree: &Path) -> Result<Vec<Conflict>> {
    let unmerged = git::run(worktree, ["diff", "--name-only", "--diff-filter=U", "-z"])?;
    unmerged
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| {
            Ok(Conflict {
                hunks: count_hunks(&fs::read(worktree.join(path)).unwrap_or_default()),
                path: path.to_owned(),
            })
        })
        .collect()
}

/// Counts conflict hunks by their start marker.
///
/// Operates on bytes: a conflicted file is as likely as not to be in the
/// middle of an encoding change, and refusing to count because the file is not
/// UTF-8 would drop exactly the information the user needs.
fn count_hunks(contents: &[u8]) -> usize {
    const MARKER: &[u8] = b"<<<<<<< ";
    contents
        .split(|byte| *byte == b'\n')
        .filter(|line| line.starts_with(MARKER))
        .count()
}

/// Content differences for every branch that was rewritten.
fn drift(worktree: &Path, moves: &[RefMove]) -> Result<Vec<Drift>> {
    let mut drifted = Vec::new();
    for moved in moves {
        // HEAD moving is the same move as its branch, and a created or deleted
        // branch has no "same content" to compare against.
        if moved.name == HEAD_KEY {
            continue;
        }
        let (Some(before), Some(after)) = (&moved.before, &moved.after) else {
            continue;
        };
        let changes = name_status(worktree, before, after)?;
        if changes.is_empty() {
            continue;
        }
        let (commits_before, commits_after) = commit_counts(worktree, before, after)?;
        drifted.push(Drift {
            reference: moved.name.clone(),
            changes,
            commits_before,
            commits_after,
        });
    }
    Ok(drifted)
}

/// `git diff --name-status` between two commits' trees.
fn name_status(worktree: &Path, before: &str, after: &str) -> Result<Vec<FileChange>> {
    let diff = git::run(worktree, ["diff", "--name-status", "-z", before, after])?;
    // -z output is NUL-separated fields, not lines: status, path, status,
    // path… (rename and copy statuses carry two paths, and their score is
    // part of the status field).
    let mut fields = diff.split('\0').filter(|field| !field.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let Some(path) = fields.next() else { break };
        let path = if status.starts_with('R') || status.starts_with('C') {
            // The destination is what the user will look at.
            fields.next().unwrap_or(path)
        } else {
            path
        };
        changes.push(FileChange {
            status: status.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(changes)
}

/// How many commits each tip carries since their common ancestor.
fn commit_counts(worktree: &Path, before: &str, after: &str) -> Result<(usize, usize)> {
    let counts = git::run(
        worktree,
        [
            "rev-list",
            "--left-right",
            "--count",
            &format!("{before}...{after}"),
        ],
    )?;
    let mut parts = counts.split_whitespace();
    let left = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let right = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    Ok((left, right))
}

/// Whether this command is supposed to land on the same content it started
/// with.
fn preserves_content(command: &[String]) -> bool {
    matches!(
        command.first().map(String::as_str),
        Some("rebase" | "cherry-pick")
    )
}

#[cfg(test)]
mod tests {
    use super::{RefMove, count_hunks, preserves_content, ref_moves};
    use std::collections::BTreeMap;

    fn state(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, sha)| ((*name).to_owned(), (*sha).to_owned()))
            .collect()
    }

    fn command(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn only_refs_that_actually_moved_are_reported() {
        let moves = ref_moves(
            &state(&[("refs/heads/main", "aaa"), ("refs/heads/feature", "bbb")]),
            &state(&[("refs/heads/main", "aaa"), ("refs/heads/feature", "ccc")]),
        );
        assert_eq!(
            moves,
            vec![RefMove {
                name: "refs/heads/feature".to_owned(),
                before: Some("bbb".to_owned()),
                after: Some("ccc".to_owned()),
            }]
        );
    }

    #[test]
    fn created_and_deleted_branches_are_moves_too() {
        let moves = ref_moves(
            &state(&[("refs/heads/gone", "aaa")]),
            &state(&[("refs/heads/new", "bbb")]),
        );
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].name, "refs/heads/gone");
        assert_eq!(moves[0].after, None, "deleted");
        assert_eq!(moves[1].name, "refs/heads/new");
        assert_eq!(moves[1].before, None, "created");
    }

    #[test]
    fn conflict_hunks_are_counted_from_the_start_markers() {
        let conflicted = b"one\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> other\ntwo\n\
                           <<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> other\n";
        assert_eq!(count_hunks(conflicted), 2);
        assert_eq!(count_hunks(b"nothing here\n"), 0);
    }

    #[test]
    fn a_line_that_merely_starts_with_angle_brackets_is_not_a_conflict() {
        // Documentation and diff output are full of these.
        assert_eq!(count_hunks(b"<<<<<<<<<<<<<< a ruler\n"), 0);
        assert_eq!(
            count_hunks(b"<<<<<<<\n"),
            0,
            "the marker includes its space"
        );
    }

    #[test]
    fn invalid_utf8_does_not_hide_a_conflict() {
        let mut conflicted = b"<<<<<<< HEAD\n".to_vec();
        conflicted.push(0xff);
        conflicted.push(b'\n');
        assert_eq!(count_hunks(&conflicted), 1);
    }

    #[test]
    fn only_replay_commands_are_expected_to_preserve_content() {
        assert!(preserves_content(&command(&["rebase", "-i", "main"])));
        assert!(preserves_content(&command(&["cherry-pick", "abc123"])));
        assert!(
            !preserves_content(&command(&["merge", "feature"])),
            "a merge changing content is the point of a merge"
        );
        assert!(!preserves_content(&command(&[])));
    }
}
