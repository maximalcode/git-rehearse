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
    ///
    /// Not merely "the trees differ": a rebase onto a base that has moved on
    /// differs by the base's own commits, every time, and warning about that
    /// would train people to ignore the warning. See [`Replay`].
    #[must_use]
    pub fn has_unexpected_drift(&self) -> bool {
        self.drift_expected_empty && self.drift.iter().any(|drift| drift.replay.is_suspicious())
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
    pub commits_after: usize,
    /// What happened to the individual commits when they were replayed.
    pub replay: Replay,
}

/// How the replayed commits compare with the ones they replaced.
///
/// Comparing trees alone cannot answer the question the warning wants to ask.
/// Rebasing onto a base that has moved on legitimately changes the tip's tree
/// — by exactly the base's own commits — and a warning that fires on the most
/// ordinary rebase there is would be ignored within a week. What matters is
/// whether the *commits being replayed* still do what they did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Replay {
    /// Subjects of commits whose patch is not what it was. The real signal:
    /// something — a conflict resolution, a merge driver — changed what these
    /// commits do.
    pub changed: Vec<String>,
    /// Subjects of commits that are simply gone.
    pub dropped: Vec<String>,
    /// Subjects of commits that exist only on the new side. Ordinary: these
    /// are the base's own commits, picked up by rebasing onto it.
    pub added: Vec<String>,
    /// Whether git could compare the two ranges at all. An unrelated history
    /// or a rewrite too large to pair up leaves this false, and an
    /// uncomparable rewrite is treated as suspicious rather than as fine.
    pub compared: bool,
}

impl Replay {
    /// Whether this replay changed something it should not have.
    #[must_use]
    pub fn is_suspicious(&self) -> bool {
        !self.compared || !self.changed.is_empty() || !self.dropped.is_empty()
    }
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
///
/// Public because apply recomputes this for itself rather than trusting an
/// [`Analysis`] handed to it: `git rehearse apply <id>` runs in a later
/// process against a rehearsal nobody has looked at since, and the refs it
/// moves must be derived from what is on disk right then.
#[must_use]
pub fn ref_moves(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<RefMove> {
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
            replay: replay(worktree, before, after),
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

/// Compares the commits behind the two tips, commit by commit.
///
/// `git range-diff <old>...<new>` pairs the commits either side of the common
/// ancestor by content and says, per pair, whether the patch is unchanged
/// (`=`), different (`!`), gone (`<`) or new (`>`). That is exactly the
/// question the drift warning wants answered, and git has done the hard part
/// — matching commits across a rewrite — since 2.19.
///
/// Delegating rather than reimplementing, for the same reason the graph
/// delegates to `git log --graph`: matching rewritten commits by content is
/// subtle, and a home-grown version would be subtly wrong exactly where it
/// matters.
fn replay(worktree: &Path, before: &str, after: &str) -> Replay {
    // Three dots: compare what each side has since their common ancestor.
    let Ok(listing) = git::run(
        worktree,
        [
            "range-diff",
            "--no-color",
            "--no-patch",
            &format!("{before}...{after}"),
        ],
    ) else {
        // Unrelated histories, or a rewrite git will not pair up. Unknown, and
        // unknown is treated as suspicious by Replay::is_suspicious.
        return Replay::default();
    };
    parse_range_diff(&listing)
}

/// Reads `git range-diff --no-patch` output.
///
/// Every line pairs one commit with its counterpart:
///
/// ```text
/// -:  ------- > 1:  b1a6479 main moves on
/// 1:  cbd2376 = 2:  0bb1e77 teach the parser about tabs
/// 2:  768e487 < -:  -------
/// ```
///
/// Five fixed fields — index, object name, operator, index, object name —
/// and then the subject, which the third line shows may be absent entirely.
///
/// **Validated against a grammar, not read by field number.** git publishes no
/// machine-readable form of this listing: there is no `--porcelain`, no `-z`,
/// and the columns are laid out for a person to read. Trusting a field index
/// is therefore a bet on a shape git never promised — and the bet was already
/// lost. A commit with an empty message produces a five-field line, the old
/// "at least six fields" check read that as *not a pairing line* and skipped
/// it, and so a **dropped** commit with an empty message was invisible: the
/// drift warning stayed silent about precisely the thing it exists to shout
/// about. See `a_dropped_commit_with_no_message_is_still_dropped` below.
///
/// So a line that does not match the grammar is not skipped. It abandons the
/// whole comparison — [`Replay::compared`] false, which
/// [`Replay::is_suspicious`] reports as a warning. This is the noisy direction
/// on purpose: if a future git reformats the listing, every rehearsal saying
/// "could not compare" is a bug report, whereas every rehearsal quietly saying
/// "no drift" is the silent failure this tool exists to prevent.
fn parse_range_diff(listing: &str) -> Replay {
    let mut replay = Replay {
        compared: true,
        ..Replay::default()
    };
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let Some((operator, subject)) = pairing(line) else {
            // Unreadable, therefore unknown, and unknown is not "fine".
            return Replay::default();
        };
        match operator {
            "!" => replay.changed.push(subject.to_owned()),
            "<" => replay.dropped.push(subject.to_owned()),
            ">" => replay.added.push(subject.to_owned()),
            // "=" is the good case: the commit still does what it did.
            _ => {}
        }
    }
    reconcile_by_subject(&mut replay);
    replay
}

/// Splits one pairing line into its operator and its subject.
///
/// `None` for anything that is not a pairing line in the shape above — which
/// is the signal to stop trusting the listing, not to move on to the next
/// line.
fn pairing(line: &str) -> Option<(&str, &str)> {
    let (left_index, rest) = token(line)?;
    let (left_object, rest) = token(rest)?;
    let (operator, rest) = token(rest)?;
    let (right_index, rest) = token(rest)?;
    let (right_object, subject) = token(rest)?;

    let shaped = is_index(left_index)
        && is_object(left_object)
        && matches!(operator, "=" | "!" | "<" | ">")
        && is_index(right_index)
        && is_object(right_object);
    // The subject is whatever is left, trimmed of the column padding but
    // otherwise untouched — collapsing its internal whitespace would stop it
    // matching the same commit's subject on the other side.
    shaped.then_some((operator, subject.trim()))
}

/// Splits the leading whitespace-delimited token off `text`, with the rest.
fn token(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    Some(
        text.find(char::is_whitespace)
            .map_or((text, ""), |end| text.split_at(end)),
    )
}

/// A commit's position in its range (`3:`), or `-:` where that side has no
/// commit to number.
fn is_index(field: &str) -> bool {
    match field.strip_suffix(':') {
        Some("-") => true,
        Some(digits) => !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// An abbreviated object name, or the dashes standing in for the one a missing
/// commit does not have.
fn is_object(field: &str) -> bool {
    !field.is_empty()
        && (field.bytes().all(|byte| byte.is_ascii_hexdigit())
            || field.bytes().all(|byte| byte == b'-'))
}

/// Re-pairs commits git would not pair, by subject.
///
/// When a conflict is resolved differently enough, range-diff's cost matrix
/// gives up on matching the old commit to the new one and reports a deletion
/// plus an addition. That reads as "your commit vanished", which is both
/// alarming and wrong — the commit is there, it just no longer does what it
/// did, which is precisely the case the warning is *for*.
///
/// Raising `--creation-factor` is git's documented remedy for this symptom,
/// but on the shapes this tool sees it makes range-diff pair a rewritten
/// commit against an unrelated one from the new base, which is worse than not
/// pairing at all. Matching the subject is narrower and cannot mispair: a
/// subject on both sides is the same commit, rewritten.
fn reconcile_by_subject(replay: &mut Replay) {
    let mut still_gone = Vec::new();
    for subject in std::mem::take(&mut replay.dropped) {
        if let Some(position) = replay.added.iter().position(|added| *added == subject) {
            replay.added.remove(position);
            replay.changed.push(subject);
        } else {
            still_gone.push(subject);
        }
    }
    replay.dropped = still_gone;
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
    fn a_range_diff_of_unchanged_commits_says_nothing_happened() {
        let replay = super::parse_range_diff(
            "1:  a1b2c3d = 1:  d4e5f6a teach the parser about tabs\n\
             2:  b2c3d4e = 2:  e5f6a7b fix the off-by-one\n",
        );
        assert!(replay.compared);
        assert!(!replay.is_suspicious(), "{replay:?}");
    }

    #[test]
    fn commits_picked_up_from_a_new_base_are_counted_not_flagged() {
        let replay = super::parse_range_diff(
            "-:  ------- > 1:  d4e5f6a main moves on\n\
             1:  a1b2c3d = 2:  e5f6a7b teach the parser about tabs\n",
        );
        assert_eq!(replay.added, vec!["main moves on".to_owned()]);
        assert!(
            !replay.is_suspicious(),
            "the most ordinary rebase there is: {replay:?}"
        );
    }

    #[test]
    fn a_commit_git_reports_as_changed_is_changed() {
        let replay =
            super::parse_range_diff("1:  a1b2c3d ! 1:  d4e5f6a teach the parser about tabs\n");
        assert_eq!(
            replay.changed,
            vec!["teach the parser about tabs".to_owned()]
        );
        assert!(replay.is_suspicious());
    }

    #[test]
    fn a_commit_rewritten_beyond_recognition_is_changed_not_gone() {
        // What range-diff actually prints when a conflict was resolved
        // differently enough: it gives up pairing and reports a deletion and
        // an addition. The commit is not gone; it stopped doing what it did.
        let replay = super::parse_range_diff(
            "1:  a1b2c3d < -:  ------- teach the parser about tabs\n\
             -:  ------- > 1:  d4e5f6a main moves on\n\
             -:  ------- > 2:  e5f6a7b teach the parser about tabs\n",
        );
        assert_eq!(
            replay.changed,
            vec!["teach the parser about tabs".to_owned()]
        );
        assert!(replay.dropped.is_empty(), "{replay:?}");
        assert_eq!(replay.added, vec!["main moves on".to_owned()]);
    }

    #[test]
    fn a_commit_that_really_is_gone_stays_gone() {
        let replay = super::parse_range_diff(
            "1:  a1b2c3d = 1:  d4e5f6a teach the parser about tabs\n\
             2:  b2c3d4e < -:  ------- fix the off-by-one\n",
        );
        assert_eq!(replay.dropped, vec!["fix the off-by-one".to_owned()]);
        assert!(replay.changed.is_empty(), "{replay:?}");
        assert!(replay.is_suspicious());
    }

    /// Verbatim `git range-diff --no-color --no-patch` output, git 2.50.1, for
    /// a feature branch of two commits rebased onto a base that gained one.
    ///
    /// Recorded from a real repository rather than written by hand: this is
    /// the format the parser bets on, so the fixture has to be evidence of
    /// what git prints and not a restatement of what we assumed it prints.
    const REAL_REBASE: &str = "\
-:  ------- > 1:  b1a6479 main moves on
1:  cbd2376 = 2:  0bb1e77 teach the parser about tabs
2:  768e487 = 3:  67ca312 fix the off-by-one
";

    /// Verbatim output, same git, for a branch whose first commit was dropped.
    /// That commit was created with `--allow-empty-message`, so its pairing
    /// line ends after the fifth field.
    const REAL_DROPPED_EMPTY_MESSAGE: &str = "\
1:  d9e4f6e < -:  -------
2:  1add7b2 = 1:  537a8ec keeper
";

    #[test]
    fn the_recorded_shape_of_a_real_rebase_still_parses() {
        // If a git upgrade changes this format, this fails here rather than
        // silently downgrading the drift warning to "nothing to report".
        let replay = super::parse_range_diff(REAL_REBASE);
        assert!(replay.compared, "{replay:?}");
        assert_eq!(replay.added, vec!["main moves on".to_owned()]);
        assert!(!replay.is_suspicious(), "{replay:?}");
    }

    #[test]
    fn a_dropped_commit_with_no_message_is_still_dropped() {
        // The line is `1:  d9e4f6e < -:  -------` — five fields and no
        // subject, because the commit has no message. Counting fields read
        // that as "not a pairing line" and skipped it, so a rebase that ate a
        // commit reported no drift at all.
        let replay = super::parse_range_diff(REAL_DROPPED_EMPTY_MESSAGE);
        assert_eq!(replay.dropped, vec![String::new()], "{replay:?}");
        assert!(
            replay.is_suspicious(),
            "a commit vanished; silence here is the failure this tool exists to prevent"
        );
    }

    #[test]
    fn a_listing_in_an_unfamiliar_format_is_not_read_as_a_clean_one() {
        // Whatever a future git might print, it is not this parser's grammar,
        // and guessing at it would be worse than admitting we cannot tell.
        for listing in [
            "commit a1b2c3d was rewritten as d4e5f6a\n",
            "1:  a1b2c3d ~ 1:  d4e5f6a an operator we do not know\n",
            "1:  a1b2c3d = 1:  not-a-sha subject\n",
            "one:  a1b2c3d = 1:  d4e5f6a subject\n",
            "1:  a1b2c3d =\n",
        ] {
            let replay = super::parse_range_diff(listing);
            assert!(
                !replay.compared,
                "parsed {listing:?} as if it understood it"
            );
            assert!(replay.is_suspicious());
        }
    }

    #[test]
    fn one_unreadable_line_discredits_the_whole_listing() {
        // Not "skip that line and report the rest": a listing we can only
        // partly read is one whose unread part may hold the drift.
        let replay = super::parse_range_diff(
            "1:  a1b2c3d = 1:  d4e5f6a teach the parser about tabs\n\
             something else entirely\n",
        );
        assert!(!replay.compared, "{replay:?}");
    }

    #[test]
    fn a_subject_keeps_the_whitespace_inside_it() {
        // Subjects are matched against each other by reconcile_by_subject, so
        // normalising one side's spacing would stop a rewritten commit pairing
        // with itself.
        let replay = super::parse_range_diff("1:  a1b2c3d ! 1:  d4e5f6a two  spaces\tand a tab\n");
        assert_eq!(replay.changed, vec!["two  spaces\tand a tab".to_owned()]);
    }

    #[test]
    fn a_comparison_git_could_not_make_is_suspicious_by_default() {
        assert!(
            super::Replay::default().is_suspicious(),
            "an unknown rewrite must not pass for a safe one"
        );
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
