//! The machine-readable report — what `--json` prints.
//!
//! **Everything in this module is API.** The types below are deliberately
//! *not* the ones in [`crate::analyze`], even though several are shaped like
//! them. Those exist to serve the terminal report and change whenever it does;
//! deriving `Serialize` on them would mean a rename in a private struct
//! silently rewriting a published schema. Duplicating the shape here is the
//! point: it makes the wire format something a person has to change on
//! purpose, and leaves `analyze.rs` free to be refactored.
//!
//! `meta.json` derives `Serialize` on its own types for the opposite reason —
//! it is a private on-disk format, versioned by [`crate::sandbox::META_SCHEMA`]
//! and read only by us.
//!
//! Two rules the callers have to keep:
//!
//! 1. **One document on stdout, and nothing else.** Notices, prompts and
//!    directions go into a field or to stderr. This is the same discipline
//!    MCP's stdio transport imposes, which is not an accident: if the server in
//!    SCOPE's v2 ever gets built, a `--json` run that already respects it is
//!    most of the work.
//! 2. **Failures are documents too.** A caller that parses JSON on success and
//!    gets English on failure has to parse English anyway, so every exit path
//!    emits one — see [`Failure`].
//!
//! The before/after graphs are deliberately absent. They are `git log --graph`
//! output, drawn for a person; a caller that wants the shape of the history has
//! the commit ids here and a git of its own.

use serde::Serialize;

use crate::analyze::{Analysis, Commit, Conflict, Drift, FileChange, RefMove, Replay};
use crate::apply::Applied;
use crate::execute::Outcome;
use crate::report::Choice;
use crate::sandbox::{Meta, Sandbox, Status};

/// Version of the document below.
///
/// Bump on any change a caller could notice: a removed field, a renamed one, a
/// changed meaning. Adding an optional field is not such a change.
pub const SCHEMA: u32 = 1;

/// How the rehearsed command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    /// Finished, nothing left in progress.
    Clean,
    /// Stopped part-way — a conflict, or an interactive `break`.
    Stopped,
    /// Git refused the command outright.
    Failed,
}

/// What became of the rehearsal afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Transplanted into the real repository.
    Applied,
    /// Left in the cache; the `id` can be used again.
    Kept,
    /// The sandbox is gone. `id` and `sandbox` describe something that no
    /// longer exists — recorded so a caller can tell this apart from `kept`.
    Discarded,
}

/// One ref, before and after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ref {
    /// Full ref name, or `HEAD`.
    pub name: String,
    /// Where it pointed before; absent if the command created it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Where it points now; absent if the command deleted it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// A commit, identified and described.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitRef {
    pub sha: String,
    pub subject: String,
}

/// An unmerged path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictEntry {
    pub path: String,
    /// Conflict hunks git left in the file. Zero means unmerged but without
    /// markers — a binary file, or an add/add or delete/modify conflict.
    pub hunks: usize,
}

/// One file in a diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileEntry {
    /// Git's status letter: `M`, `A`, `D`, `R`…
    pub status: String,
    pub path: String,
}

/// What happened to the individual commits when they were replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayReport {
    /// Subjects of commits whose patch is not what it was — the real signal.
    pub changed: Vec<String>,
    /// Subjects of commits that are simply gone.
    pub dropped: Vec<String>,
    /// Subjects present only on the new side. Ordinary: these are the base's
    /// own commits, picked up by rebasing onto it.
    pub added: Vec<String>,
    /// Whether git could pair the two ranges up at all. False is treated as
    /// suspicious rather than as fine.
    pub compared: bool,
}

/// The content difference between a branch's old and new tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriftEntry {
    pub reference: String,
    pub files: Vec<FileEntry>,
    pub commits_before: usize,
    pub commits_after: usize,
    pub replay: ReplayReport,
}

/// What an apply moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppliedReport {
    /// Refs moved, created or deleted in the real repository.
    pub refs: Vec<Ref>,
    /// The branch whose worktree was reset, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_reset: Option<String>,
    /// Where the pre-state was written down, so it can be undone by hand.
    pub undo: String,
}

/// The rehearsal report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub schema: u32,
    /// The rehearsal id — an unambiguous prefix of it is what `continue`,
    /// `apply` and `discard` take.
    pub id: String,
    /// The real repository this was rehearsed against.
    pub repository: String,
    /// The sandbox worktree. Present whatever the decision, but only usable
    /// when `decision` is not `discarded`.
    pub sandbox: String,
    /// The command, as git received it: `["rebase", "main"]`.
    pub command: Vec<String>,
    pub outcome: OutcomeKind,
    /// The process exit code, repeated here so a caller reading a captured
    /// document does not need the process it came from.
    pub exit_code: u8,
    /// Git's own exit code, when git refused the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_exit_code: Option<i32>,
    /// Whether the stop involves unmerged paths. An interactive `break` stops
    /// without any, and calling that a conflict would be a lie.
    pub conflicted: bool,
    pub refs: Vec<Ref>,
    /// The commit being replayed when the command stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<CommitRef>,
    pub conflicts: Vec<ConflictEntry>,
    pub drift: Vec<DriftEntry>,
    /// Whether `drift` is a surprise. True only where replaying should have
    /// preserved content and did not — the warning this tool exists for.
    pub drift_unexpected: bool,
    /// Whether this rehearsal is in a state that could be applied.
    pub can_apply: bool,
    pub decision: Decision,
    /// Present only when `decision` is `applied`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<AppliedReport>,
}

/// One entry of `git rehearse --json list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub id: String,
    pub command: Vec<String>,
    pub sandbox: String,
    /// `fresh` or `kept`, as recorded in the sandbox's own metadata.
    pub status: String,
    /// Seconds since the Unix epoch.
    pub created_unix: u64,
    /// How the command ended, if it has run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<OutcomeKind>,
}

impl Entry {
    /// One listing entry for a rehearsal in the cache.
    #[must_use]
    pub fn of(sandbox: &Sandbox) -> Self {
        let meta = sandbox.meta();
        Self {
            id: meta.id.clone(),
            command: meta.command.clone(),
            sandbox: sandbox.worktree().display().to_string(),
            // Spelled out rather than derived from `Status`: that enum is
            // `meta.json`'s, and a rename there must not reach the wire.
            status: match meta.status {
                Status::Fresh => "fresh",
                Status::Kept => "kept",
            }
            .to_owned(),
            created_unix: meta.created_unix,
            outcome: meta.result.as_ref().map(kind_of),
        }
    }
}

/// `git rehearse --json list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub schema: u32,
    pub rehearsals: Vec<Entry>,
    /// Rehearsals deleted by the age-out sweep this run performed.
    pub pruned: Vec<String>,
}

/// `git rehearse --json apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyResult {
    pub schema: u32,
    pub id: String,
    pub repository: String,
    pub exit_code: u8,
    pub applied: AppliedReport,
}

/// `git rehearse --json discard`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscardResult {
    pub schema: u32,
    /// The ids that are now gone.
    pub discarded: Vec<String>,
    pub exit_code: u8,
}

/// Why a run failed.
///
/// `kind` maps onto the exit code, which is the same distinction SCOPE fixes:
/// a refusal is the tool working correctly and declining, an internal error is
/// a bug of ours. A caller should retry neither, but should report them very
/// differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Exit 4 — dirty worktree, refs moved, unsupported repository.
    Refused,
    /// Exit 1 — a bug in git-rehearse.
    Internal,
}

/// The document printed instead of a report when a run fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub schema: u32,
    pub kind: FailureKind,
    /// The same text a person would have seen on stderr.
    pub message: String,
    pub exit_code: u8,
}

impl Failure {
    /// The failure document for an error and the exit code it produced.
    #[must_use]
    pub fn new(message: String, exit_code: u8) -> Self {
        Self {
            schema: SCHEMA,
            kind: if exit_code == crate::cli::exit::REFUSED {
                FailureKind::Refused
            } else {
                FailureKind::Internal
            },
            message,
            exit_code,
        }
    }
}

impl Report {
    /// Builds the report document.
    ///
    /// Takes the sandbox rather than just its metadata because the document
    /// names the sandbox path, and that is the one thing a caller cannot work
    /// out for itself.
    #[must_use]
    pub fn new(
        sandbox: &Sandbox,
        analysis: &Analysis,
        outcome: &Outcome,
        exit_code: u8,
        can_apply: bool,
        choice: Choice,
        applied: Option<&Applied>,
    ) -> Self {
        let meta: &Meta = sandbox.meta();
        Self {
            schema: SCHEMA,
            id: meta.id.clone(),
            repository: meta.repo_path.display().to_string(),
            sandbox: sandbox.worktree().display().to_string(),
            command: meta.command.clone(),
            outcome: kind_of(outcome),
            exit_code,
            git_exit_code: match outcome {
                Outcome::Failed { code } => *code,
                Outcome::Clean | Outcome::Stopped { .. } => None,
            },
            conflicted: matches!(outcome, Outcome::Stopped { conflicts: true }),
            refs: analysis.ref_moves.iter().map(reference).collect(),
            stopped_at: analysis.stopped_at.as_ref().map(commit),
            conflicts: analysis.conflicts.iter().map(conflict).collect(),
            drift: analysis.drift.iter().map(drift).collect(),
            drift_unexpected: analysis.has_unexpected_drift(),
            can_apply,
            decision: match choice {
                Choice::Apply => Decision::Applied,
                Choice::Keep => Decision::Kept,
                Choice::Discard => Decision::Discarded,
            },
            applied: applied.map(applied_report),
        }
    }
}

/// The document kind for an outcome.
fn kind_of(outcome: &Outcome) -> OutcomeKind {
    match outcome {
        Outcome::Clean => OutcomeKind::Clean,
        Outcome::Stopped { .. } => OutcomeKind::Stopped,
        Outcome::Failed { .. } => OutcomeKind::Failed,
    }
}

fn reference(moved: &RefMove) -> Ref {
    Ref {
        name: moved.name.clone(),
        before: moved.before.clone(),
        after: moved.after.clone(),
    }
}

fn commit(found: &Commit) -> CommitRef {
    CommitRef {
        sha: found.sha.clone(),
        subject: found.subject.clone(),
    }
}

fn conflict(found: &Conflict) -> ConflictEntry {
    ConflictEntry {
        path: found.path.clone(),
        hunks: found.hunks,
    }
}

fn file(change: &FileChange) -> FileEntry {
    FileEntry {
        status: change.status.clone(),
        path: change.path.clone(),
    }
}

fn replay(found: &Replay) -> ReplayReport {
    ReplayReport {
        changed: found.changed.clone(),
        dropped: found.dropped.clone(),
        added: found.added.clone(),
        compared: found.compared,
    }
}

fn drift(found: &Drift) -> DriftEntry {
    DriftEntry {
        reference: found.reference.clone(),
        files: found.changes.iter().map(file).collect(),
        commits_before: found.commits_before,
        commits_after: found.commits_after,
        replay: replay(&found.replay),
    }
}

/// The applied-refs document.
#[must_use]
pub fn applied_report(applied: &Applied) -> AppliedReport {
    AppliedReport {
        refs: applied.moved.iter().map(reference).collect(),
        worktree_reset: applied.reset.clone(),
        undo: applied.undo.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, Failure, FailureKind, OutcomeKind, Report, SCHEMA};
    use crate::analyze::{Analysis, Commit, Conflict};
    use crate::cli::exit;
    use crate::execute::Outcome;

    fn analysis() -> Analysis {
        Analysis {
            ref_moves: Vec::new(),
            stopped_at: Some(Commit {
                sha: "0a7bfa18".to_owned(),
                subject: "raise the timeout".to_owned(),
            }),
            conflicts: vec![Conflict {
                path: "config.toml".to_owned(),
                hunks: 1,
            }],
            drift: Vec::new(),
            drift_expected_empty: true,
        }
    }

    #[test]
    fn the_schema_is_stated_in_every_document() {
        // A caller reading a captured document has nothing else to key on.
        assert_eq!(SCHEMA, 1);
        assert_eq!(Failure::new("no".to_owned(), exit::REFUSED).schema, SCHEMA);
    }

    #[test]
    fn a_refusal_and_a_bug_are_different_kinds() {
        // Neither is retryable, but they are reported to a human very
        // differently: one is the tool working, the other is the tool broken.
        assert_eq!(
            Failure::new("dirty".to_owned(), exit::REFUSED).kind,
            FailureKind::Refused
        );
        assert_eq!(
            Failure::new("oops".to_owned(), exit::INTERNAL).kind,
            FailureKind::Internal
        );
    }

    #[test]
    fn an_interactive_break_is_stopped_but_not_conflicted() {
        // `break` stops with no unmerged paths. Reporting that as a conflict
        // would send a caller looking for files that are not there.
        let json = serde_json::to_value(report(&Outcome::Stopped { conflicts: false }))
            .expect("serialises");
        assert_eq!(json["outcome"], "stopped");
        assert_eq!(json["conflicted"], false);
    }

    #[test]
    fn absent_things_are_absent_rather_than_null() {
        let json = serde_json::to_value(report(&Outcome::Stopped { conflicts: true }))
            .expect("serialises");
        assert!(
            json.get("applied").is_none(),
            "nothing was applied, so there is no applied object: {json}"
        );
        assert!(
            json.get("git_exit_code").is_none(),
            "git did not refuse anything: {json}"
        );
        assert_eq!(json["stopped_at"]["sha"], "0a7bfa18");
        assert_eq!(json["conflicts"][0]["hunks"], 1);
    }

    #[test]
    fn a_git_failure_carries_gits_own_exit_code() {
        let json =
            serde_json::to_value(report(&Outcome::Failed { code: Some(128) })).expect("serialises");
        assert_eq!(json["outcome"], "failed");
        assert_eq!(json["git_exit_code"], 128);
    }

    /// A report that does not need a sandbox on disk — the fields under test
    /// here all come from the analysis and the outcome.
    fn report(outcome: &Outcome) -> Report {
        Report {
            schema: SCHEMA,
            id: "1786281796-00".to_owned(),
            repository: "/repo".to_owned(),
            sandbox: "/cache/1786281796-00/sandbox".to_owned(),
            command: vec!["rebase".to_owned(), "main".to_owned()],
            outcome: super::kind_of(outcome),
            exit_code: exit::STOPPED,
            git_exit_code: match outcome {
                Outcome::Failed { code } => *code,
                Outcome::Clean | Outcome::Stopped { .. } => None,
            },
            conflicted: matches!(outcome, Outcome::Stopped { conflicts: true }),
            refs: Vec::new(),
            stopped_at: analysis().stopped_at.as_ref().map(super::commit),
            conflicts: analysis().conflicts.iter().map(super::conflict).collect(),
            drift: Vec::new(),
            drift_unexpected: false,
            can_apply: false,
            decision: Decision::Kept,
            applied: None,
        }
    }

    #[test]
    fn outcome_names_are_the_wire_format() {
        // Renaming any of these is a schema change, not a refactor.
        for (outcome, name) in [
            (Outcome::Clean, "clean"),
            (Outcome::Stopped { conflicts: true }, "stopped"),
            (Outcome::Failed { code: None }, "failed"),
        ] {
            assert_eq!(
                serde_json::to_value(super::kind_of(&outcome)).expect("serialises"),
                name
            );
        }
        assert_eq!(
            serde_json::to_value(OutcomeKind::Clean).expect("serialises"),
            "clean"
        );
    }
}
