//! Durable apply journals and crash recovery.
//!
//! An apply is deliberately observable as a small state machine. The journal
//! is written and synced before any repository mutation, then advanced only
//! after each mutation has been observed. A repository-local OS lock is acquired
//! before the journal is read and stays with the process through phase updates
//! and cleanup, so a live operation's endpoint cannot be replaced or mistaken
//! for an abandoned journal. Recovery always compares the journal's expected
//! refs with the real repository before choosing an action; it never re-runs
//! the rehearsed command or an entire apply.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::analyze::RefMove;
use crate::carry;
use crate::collision;
use crate::preflight::HEAD_KEY;
use crate::{Error, Result, git};

mod state;
use state::{Endpoint, EndpointMatches, Observed};

/// The file in the repository's Git directory that describes an apply in flight.
pub const JOURNAL_FILE: &str = "rehearse-apply";
const JOURNAL_SCHEMA: u32 = 1;
const LOCK_SUFFIX: &str = "lock";

/// The durable point reached by an apply process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The complete operation description is durable, before ref mutation.
    Prepared,
    /// The ref transaction has landed; the worktree may still be old.
    RefsApplied,
    /// The checked-out branch and its index/worktree have been updated.
    WorktreeUpdated,
    /// Apply and its final journal write have completed.
    Complete,
    /// Rollback was requested durably, before restoring refs or the worktree.
    RollingBack,
}

impl Phase {
    fn storage_failure_stage(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::RefsApplied => "refs-applied",
            Self::WorktreeUpdated => "worktree-updated",
            Self::Complete => "complete",
            Self::RollingBack => "rolling-back",
        }
    }
}

/// What the journal and actual repository state say about an interrupted apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No user-visible ref mutation has landed.
    BeforeRefChange,
    /// Refs landed and can be completed or rolled back after checks.
    AfterRefChange,
    /// The recorded apply is complete and needs no mutation.
    Complete,
    /// A recorded rollback can be resumed from either verified endpoint.
    RollingBack,
    /// Actual state does not match either safe endpoint.
    Ambiguous,
}

/// A safe action for a journaled operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Query only.
    Inspect,
    /// Finish a ref-applied operation by updating the worktree.
    Complete,
    /// Put refs and the clean worktree back, where that is provable.
    Rollback,
}

/// The original mutation whose completion or rollback recovery controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Transplant the rehearsed result into the repository.
    #[default]
    Apply,
    /// Restore the refs saved by the previous Apply.
    Undo,
}

impl Operation {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Undo => "undo",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JournalRef {
    name: String,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Journal {
    schema: u32,
    rehearsal: String,
    origin: String,
    checkout: Option<String>,
    refs: Vec<JournalRef>,
    anchor: String,
    worktree_before: String,
    #[serde(default)]
    worktree_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_before: Option<String>,
    previous_undo: Option<Vec<u8>>,
    expected_undo: Option<Vec<u8>>,
    rollback_undo: Option<Vec<u8>>,
    #[serde(default)]
    operation: Operation,
    phase: Phase,
}

/// Integrity covers the known payload; unknown optional fields remain tolerable.
#[derive(Serialize, Deserialize)]
struct StoredJournal {
    #[serde(flatten)]
    journal: Journal,
    checksum: String,
}

/// Public description returned by inspection and recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inspection {
    pub operation: Operation,
    pub rehearsal: String,
    pub origin: String,
    pub phase: Phase,
    pub state: State,
    pub can_complete: bool,
    pub can_rollback: bool,
    pub journal: String,
}

/// The result of a recovery action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub inspection: Inspection,
    pub action: Action,
}

/// Exclusive ownership of the repository's recovery state.
///
/// The lock is an OS-managed lock on a stable file. The file is deliberately
/// never renamed or removed: replacing a pathname while another process holds
/// its old inode would let the replacement operation run beside that owner.
pub struct Lock {
    file: File,
    journal_path: PathBuf,
}

impl Lock {
    fn for_journal(journal_path: &Path) -> Result<Self> {
        let lock_path = lock_path(journal_path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(Error::io(&lock_path))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                file,
                journal_path: journal_path.to_owned(),
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::Refused(format!(
                "apply recovery is blocked: another process owns the live apply journal at {}; \
                 no repository mutation was performed",
                lock_path.display()
            ))),
            Err(std::fs::TryLockError::Error(error)) => Err(Error::io(&lock_path)(error)),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Returns the journal path for a repository.
pub fn path(repo: &Path) -> Result<PathBuf> {
    Ok(git_dir(repo)?.join(JOURNAL_FILE))
}

/// Acquires exclusive recovery ownership for the repository.
pub fn acquire(repo: &Path) -> Result<Lock> {
    let journal_path = path(repo)?;
    let lock = Lock::for_journal(&journal_path)?;
    if journal_path.exists() {
        pause_for_test("after-lock");
    }
    Ok(lock)
}

/// Refuses a mutation while an unfinished apply needs a decision. A complete
/// journal is reaped only while the stable OS lock is held, which proves no
/// other process can publish or mutate recovery state concurrently.
pub fn ensure_clear(repo: &Path) -> Result<()> {
    let lock = acquire(repo)?;
    ensure_clear_locked(repo, &lock)
}

/// Checks and, when safe, clears the current journal while `lock` is held.
pub fn ensure_clear_locked(repo: &Path, lock: &Lock) -> Result<()> {
    if path(repo)? != lock.journal_path {
        return Err(Error::Refused(
            "apply recovery is blocked: the recovery lock belongs to another repository; no repository mutation was performed".to_owned(),
        ));
    }
    let journal_path = &lock.journal_path;
    if !journal_path.exists() {
        return Ok(());
    }
    let journal = read(journal_path)?;
    let inspection = inspect_journal(repo, &journal, journal_path)?;
    if inspection.state == State::Complete {
        remove_undo_for(repo, &journal)?;
        remove_journal(journal_path)?;
        return Ok(());
    }
    Err(blocked(&inspection))
}

/// Writes the complete operation description before the first mutation.
pub fn prepare(
    repo: &Path,
    record: &crate::undo::Record,
    checkout: Option<&str>,
    anchor: &str,
    worktree_before: &str,
) -> Result<PathBuf> {
    let lock = acquire(repo)?;
    prepare_locked(repo, &lock, record, checkout, anchor, worktree_before)
}

/// Writes an apply journal while `lock` remains held by the caller.
pub fn prepare_locked(
    repo: &Path,
    lock: &Lock,
    record: &crate::undo::Record,
    checkout: Option<&str>,
    anchor: &str,
    worktree_before: &str,
) -> Result<PathBuf> {
    prepare_operation_locked(
        repo,
        lock,
        &record.rehearsal,
        &record.refs,
        checkout,
        anchor,
        worktree_before,
        Operation::Apply,
        Some(crate::undo::render(record).into_bytes()),
    )
}

/// Writes the complete Undo operation description before restoring a ref.
pub fn prepare_undo(
    repo: &Path,
    rehearsal: &str,
    moved: &[RefMove],
    checkout: Option<&str>,
    worktree_before: &str,
) -> Result<PathBuf> {
    let lock = acquire(repo)?;
    prepare_undo_locked(repo, &lock, rehearsal, moved, checkout, worktree_before)
}

/// Writes an Undo journal while `lock` remains held by the caller.
pub fn prepare_undo_locked(
    repo: &Path,
    lock: &Lock,
    rehearsal: &str,
    moved: &[RefMove],
    checkout: Option<&str>,
    worktree_before: &str,
) -> Result<PathBuf> {
    prepare_operation_locked(
        repo,
        lock,
        rehearsal,
        moved,
        checkout,
        "",
        worktree_before,
        Operation::Undo,
        None,
    )
}

#[allow(clippy::too_many_arguments)] // Journal fields and lock stay explicit at this durable seam.
fn prepare_operation_locked(
    repo: &Path,
    lock: &Lock,
    rehearsal: &str,
    moved: &[RefMove],
    checkout: Option<&str>,
    anchor: &str,
    worktree_before: &str,
    operation: Operation,
    expected_undo: Option<Vec<u8>>,
) -> Result<PathBuf> {
    let journal_path = &lock.journal_path;
    ensure_clear_locked(repo, lock)?;
    if !anchor.is_empty() && !git::refs(repo, anchor, 0)?.is_empty() {
        return Err(Error::Refused(
            "this rehearsal's recovery anchor is already owned by an earlier apply; create a new rehearsal before applying. No repository mutation was performed".to_owned(),
        ));
    }
    let previous_path = git_dir(repo)?.join(crate::undo::UNDO_FILE);
    let previous_undo = match fs::read(&previous_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(Error::Refused(format!(
                "could not read the existing undo record at {} before writing the apply journal: \
                 {error}. Recovery is blocked and no repository mutation was performed.",
                previous_path.display()
            )));
        }
    };
    let refs = moved
        .iter()
        .filter(|moved| moved.name != HEAD_KEY)
        .map(|moved| JournalRef {
            name: moved.name.clone(),
            before: moved.before.clone(),
            after: moved.after.clone(),
        })
        .collect();
    let local_before = if checkout.is_some() {
        carry::snapshot(repo)?.map(|carry| carry.snapshot)
    } else {
        None
    };
    if let Some(snapshot) = &local_before {
        if !worktree_result_matches(repo, snapshot, Some(&format!("{snapshot}^2"))) {
            return Err(Error::Refused("the original index or tracked files cannot be verified against their snapshot; apply was not started".to_owned()));
        }
        git::run(
            repo,
            [
                "update-ref",
                &format!("refs/rehearse/{rehearsal}-original"),
                snapshot,
            ],
        )?;
    }
    let journal = Journal {
        schema: JOURNAL_SCHEMA,
        rehearsal: rehearsal.to_owned(),
        origin: repo.display().to_string(),
        checkout: checkout.map(str::to_owned),
        refs,
        anchor: anchor.to_owned(),
        worktree_before: worktree_before.to_owned(),
        worktree_after: None,
        local_before,
        previous_undo,
        expected_undo,
        rollback_undo: None,
        operation,
        phase: Phase::Prepared,
    };
    if let Err(error) = write_atomic_exclusive(journal_path, &journal) {
        return Err(Error::Refused(format!(
            "could not durably publish apply journal at {}: {error}. Recovery is blocked and no \
         repository mutation was performed.",
            journal_path.display()
        )));
    }
    Ok(journal_path.clone())
}

/// Records the exact carried worktree endpoint before refs are mutated.
pub fn set_worktree_after(journal_path: &Path, result: Option<&str>) -> Result<()> {
    let lock = Lock::for_journal(journal_path)?;
    set_worktree_after_locked(journal_path, &lock, result)
}

/// Records the carried endpoint while `lock` remains held by the caller.
pub fn set_worktree_after_locked(
    journal_path: &Path,
    lock: &Lock,
    result: Option<&str>,
) -> Result<()> {
    let mut journal = read(journal_path)?;
    journal.worktree_after = result.map(str::to_owned);
    write_owned(journal_path, lock, &journal, "worktree-after").map_err(|error| {
        Error::Refused(format!(
            "could not durably record the rehearsed worktree at {}: {error}. Recovery is blocked; \
         inspect the real refs before taking any action.",
            journal_path.display()
        ))
    })
}

/// Advances a prepared journal after the ref transaction.
pub fn refs_applied(journal_path: &Path) -> Result<()> {
    let lock = Lock::for_journal(journal_path)?;
    refs_applied_locked(journal_path, &lock)
}

/// Advances the journal while `lock` remains held by the caller.
pub fn refs_applied_locked(journal_path: &Path, lock: &Lock) -> Result<()> {
    update_phase(journal_path, lock, Phase::RefsApplied)
}

/// Advances a journal after the checked-out worktree has been updated.
pub fn worktree_updated(journal_path: &Path) -> Result<()> {
    let lock = Lock::for_journal(journal_path)?;
    worktree_updated_locked(journal_path, &lock)
}

/// Advances the journal while `lock` remains held by the caller.
pub fn worktree_updated_locked(journal_path: &Path, lock: &Lock) -> Result<()> {
    update_phase(journal_path, lock, Phase::WorktreeUpdated)
}

/// Marks the operation complete, leaving cleanup to the caller after all user
/// visible work has succeeded.
pub fn complete(journal_path: &Path) -> Result<()> {
    let lock = Lock::for_journal(journal_path)?;
    complete_locked(journal_path, &lock)
}

/// Marks the journal complete while `lock` remains held by the caller.
pub fn complete_locked(journal_path: &Path, lock: &Lock) -> Result<()> {
    update_phase(journal_path, lock, Phase::Complete)
}

/// Removes a completed journal. Call only after the actual operation is done.
pub fn forget(journal_path: &Path) -> Result<()> {
    let lock = Lock::for_journal(journal_path)?;
    forget_locked(journal_path, &lock)
}

/// Removes a completed journal while `lock` remains held by the caller.
pub fn forget_locked(journal_path: &Path, _lock: &Lock) -> Result<()> {
    if journal_path.exists() {
        remove_journal(journal_path)?;
    }
    Ok(())
}

/// Inspect the repository's current recovery state, if a journal exists.
pub fn inspect(repo: &Path) -> Result<Option<Inspection>> {
    let lock = acquire(repo)?;
    inspect_locked(repo, &lock)
}

/// Inspects recovery state while `lock` is held by the caller.
pub fn inspect_locked(repo: &Path, lock: &Lock) -> Result<Option<Inspection>> {
    let journal_path = &lock.journal_path;
    if !journal_path.exists() {
        return Ok(None);
    }
    let journal = read(journal_path)?;
    inspect_journal(repo, &journal, journal_path).map(Some)
}

/// Complete or roll back a journal based on observed state.
pub fn recover(repo: &Path, action: Action) -> Result<Recovered> {
    recover_for(repo, action, None)
}

/// Complete or roll back a journal, optionally insisting on a rehearsal id.
pub fn recover_for(
    repo: &Path,
    action: Action,
    expected_rehearsal: Option<&str>,
) -> Result<Recovered> {
    let lock = acquire(repo)?;
    let journal_path = &lock.journal_path;
    if !journal_path.try_exists().map_err(Error::io(journal_path))? {
        return Err(Error::Refused(
            "no interrupted apply requires recovery; no recovery action was done".to_owned(),
        ));
    }
    recover_locked(repo, &lock, action, expected_rehearsal)
}

/// Completes or rolls back while `lock` remains held by the caller.
fn recover_locked(
    repo: &Path,
    lock: &Lock,
    action: Action,
    expected_rehearsal: Option<&str>,
) -> Result<Recovered> {
    let journal_path = &lock.journal_path;
    let mut journal = read(journal_path)?;
    let mut inspection = inspect_journal(repo, &journal, journal_path)?;
    if let Some(expected) = expected_rehearsal
        && !journal.rehearsal.starts_with(expected)
    {
        return Err(Error::Refused(format!(
            "the apply journal does not belong to rehearsal {expected}; no recovery action was done"
        )));
    }
    match action {
        Action::Inspect => Ok(Recovered { inspection, action }),
        Action::Complete => {
            if inspection.state != State::Complete
                && (inspection.state != State::AfterRefChange || !inspection.can_complete)
            {
                return Err(blocked(&inspection));
            }
            journal = read(journal_path)?;
            inspection = inspect_journal(repo, &journal, journal_path)?;
            if inspection.state == State::Complete {
                remove_undo_for(repo, &journal)?;
                forget_locked(journal_path, lock)?;
                return Ok(Recovered { inspection, action });
            }
            if inspection.state != State::AfterRefChange || !inspection.can_complete {
                return Err(blocked(&inspection));
            }
            let Some(branch) = journal.checkout.as_deref() else {
                // Detached HEAD worktrees are intentionally not reset by apply.
                // Refs are already the complete user-visible result.
                remove_undo_for(repo, &journal)?;
                complete_locked(journal_path, lock)?;
                forget_locked(journal_path, lock)?;
                return Ok(Recovered { inspection, action });
            };
            ensure_checkout(repo, branch)?;
            ensure_known_worktree(repo, &journal)?;
            update_worktree(repo, &journal)?;
            remove_undo_for(repo, &journal)?;
            complete_locked(journal_path, lock)?;
            forget_locked(journal_path, lock)?;
            let _ = branch;
            Ok(Recovered { inspection, action })
        }
        Action::Rollback => rollback_locked(repo, lock, journal, inspection),
    }
}

/// Persist the requested direction before the first inverse mutation.
fn rollback_locked(
    repo: &Path,
    lock: &Lock,
    mut journal: Journal,
    inspection: Inspection,
) -> Result<Recovered> {
    if !inspection.can_rollback {
        return Err(blocked(&inspection));
    }
    if journal.phase != Phase::RollingBack {
        let current_undo = read_undo(repo)?;
        if !undo_matches_snapshot(&journal, inspection.state, current_undo.as_deref()) {
            return Err(blocked(&inspection));
        }
        journal.rollback_undo = current_undo;
        journal.phase = Phase::RollingBack;
        write_phase(&lock.journal_path, lock, &journal)?;
    }
    crate::test_hooks::abort("GIT_REHEARSE_ABORT_RECOVERY_AT", "after-rollback-journal");

    // A previous attempt may have restored the refs already. Only perform
    // the inverse transaction when all refs still match its expected values.
    let refs = state_of(repo)?;
    let restored = refs_match(&refs, &journal, Endpoint::Before);
    if !restored {
        if !refs_match(&refs, &journal, Endpoint::After) {
            return Err(blocked(&inspection));
        }
        if let Some(branch) = journal.checkout.as_deref() {
            ensure_checkout(repo, branch)?;
            ensure_rollback_worktree(repo, &journal)?;
        } else {
            ensure_checkout_not_moved(repo, &journal)?;
        }
        restore_refs(repo, &journal)?;
    }
    crate::test_hooks::abort("GIT_REHEARSE_ABORT_RECOVERY_AT", "after-rollback-refs");
    if let Some(branch) = journal.checkout.as_deref() {
        ensure_checkout(repo, branch)?;
        if !refs_match(&state_of(repo)?, &journal, Endpoint::Before) {
            return Err(blocked(&inspection));
        }
        ensure_rollback_worktree(repo, &journal)?;
        if !worktree_matches_journal(repo, &journal, Endpoint::Before) {
            if let Some(snapshot) = &journal.local_before {
                carry::restore_snapshot(repo, snapshot)?;
            } else {
                git::run(repo, ["reset", "--hard", "--quiet"])?;
            }
        }
    }
    crate::test_hooks::abort("GIT_REHEARSE_ABORT_RECOVERY_AT", "after-rollback-worktree");
    restore_previous(repo, &journal)?;
    forget_locked(&lock.journal_path, lock)?;
    remove_anchor(repo, &journal)?;
    Ok(Recovered {
        inspection,
        action: Action::Rollback,
    })
}

fn ensure_rollback_worktree(repo: &Path, journal: &Journal) -> Result<()> {
    if worktree_matches_journal(repo, journal, Endpoint::Before) {
        return Ok(());
    }
    ensure_known_worktree(repo, journal)?;
    if let Some(snapshot) = &journal.local_before {
        collision::check_restore(repo, repo, snapshot)
    } else {
        let branch = journal.checkout.as_deref().ok_or_else(|| {
            Error::Refused("rollback has no recorded checkout to restore".to_owned())
        })?;
        rollback_reset_safe(repo, journal, branch)
    }
}

/// Only exact recorded endpoints or the two Git checkout steps are recoverable.
/// Partial file writes and unrelated edits remain ambiguous.
fn worktree_in_transit(repo: &Path, journal: &Journal) -> bool {
    if journal.local_before.is_none()
        || !matches!(
            journal.phase,
            Phase::Prepared | Phase::RefsApplied | Phase::RollingBack
        )
    {
        return false;
    }
    let Some(branch) = &journal.checkout else {
        return false;
    };
    if ensure_checkout(repo, branch).is_err() {
        return false;
    }
    let reset = journal
        .refs
        .iter()
        .find(|reference| reference.name == format!("refs/heads/{branch}"))
        .and_then(|reference| reference.after.as_deref());
    reset.is_some_and(|commit| worktree_at(repo, commit))
        || journal
            .worktree_after
            .as_deref()
            .is_some_and(|snapshot| worktree_at(repo, snapshot))
        || (journal.phase == Phase::RollingBack
            && journal
                .local_before
                .as_deref()
                .is_some_and(|snapshot| worktree_at(repo, snapshot)))
}

fn ensure_known_worktree(repo: &Path, journal: &Journal) -> Result<()> {
    if worktree_matches_journal(repo, journal, Endpoint::Before)
        || worktree_matches_journal(repo, journal, Endpoint::After)
        || worktree_in_transit(repo, journal)
    {
        return Ok(());
    }
    Err(Error::Refused("apply recovery is blocked: the worktree or index changed while interrupted; preserve the external work before recovering".to_owned()))
}

fn refs_match(refs: &BTreeMap<String, String>, journal: &Journal, endpoint: Endpoint) -> bool {
    journal.refs.iter().all(|reference| {
        let expected = match endpoint {
            Endpoint::Before => &reference.before,
            Endpoint::After => &reference.after,
        };
        refs.get(&reference.name) == expected.as_ref()
    })
}

fn inspect_journal(repo: &Path, journal: &Journal, journal_path: &Path) -> Result<Inspection> {
    if journal.schema != JOURNAL_SCHEMA {
        return Err(Error::Refused(format!(
            "{} uses apply journal schema {}, this build understands {JOURNAL_SCHEMA}; \
             recovery is blocked so the journal will not be overwritten",
            journal_path.display(),
            journal.schema
        )));
    }
    let origin = fs::canonicalize(&journal.origin).map_err(|_| {
        Error::Refused("apply journal origin is unavailable; recovery is blocked".to_owned())
    })?;
    if origin != fs::canonicalize(repo).map_err(Error::io(repo))? {
        return Err(Error::Refused(
            "apply journal belongs to another repository; recovery is blocked".to_owned(),
        ));
    }
    let expected_anchor = match journal.operation {
        Operation::Apply => format!("refs/rehearse/{}/", journal.rehearsal),
        Operation::Undo => String::new(),
    };
    if journal.anchor != expected_anchor {
        return Err(Error::Refused(
            "apply journal has an inconsistent recovery anchor; recovery is blocked".to_owned(),
        ));
    }
    let refs = state_of(repo)?;
    let observed_state = state::classify(
        journal.phase,
        Observed {
            refs: EndpointMatches {
                before: refs_match(&refs, journal, Endpoint::Before),
                after: refs_match(&refs, journal, Endpoint::After),
            },
            worktree: EndpointMatches {
                after: worktree_matches_journal(repo, journal, Endpoint::After),
                before: worktree_matches_journal(repo, journal, Endpoint::Before)
                    || worktree_in_transit(repo, journal),
            },
        },
    );
    let state = if undo_matches(repo, journal, observed_state)? {
        observed_state
    } else {
        State::Ambiguous
    };
    let can_complete = state == State::AfterRefChange && completion_safe(repo, journal);
    let can_rollback = matches!(
        state,
        State::BeforeRefChange | State::AfterRefChange | State::Complete | State::RollingBack
    ) && (journal.worktree_after.is_none() || journal.local_before.is_some())
        && rollback_safe(repo, journal, state);
    Ok(Inspection {
        operation: journal.operation,
        rehearsal: journal.rehearsal.clone(),
        origin: journal.origin.clone(),
        phase: journal.phase,
        state,
        can_complete,
        can_rollback,
        journal: journal_path.display().to_string(),
    })
}

fn undo_matches(repo: &Path, journal: &Journal, state: State) -> Result<bool> {
    Ok(undo_matches_snapshot(
        journal,
        state,
        read_undo(repo)?.as_deref(),
    ))
}

fn read_undo(repo: &Path) -> Result<Option<Vec<u8>>> {
    let path = git_dir(repo)?.join(crate::undo::UNDO_FILE);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(&path)(error)),
    }
}

fn undo_matches_snapshot(journal: &Journal, state: State, current: Option<&[u8]>) -> bool {
    if state == State::RollingBack {
        return current == journal.rollback_undo.as_deref()
            || current == journal.previous_undo.as_deref();
    }
    match journal.operation {
        Operation::Apply => {
            current == journal.expected_undo.as_deref()
                || ((state == State::BeforeRefChange
                    || (journal.phase == Phase::Prepared && journal.refs.is_empty()))
                    && current == journal.previous_undo.as_deref())
        }
        Operation::Undo => {
            current == journal.previous_undo.as_deref()
                || (state == State::Complete && current.is_none())
        }
    }
}

fn worktree_matches_journal(repo: &Path, journal: &Journal, endpoint: Endpoint) -> bool {
    let Some(branch) = journal.checkout.as_deref() else {
        return true;
    };
    let checkout_matches = git::run(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .is_ok_and(|current| current == branch);
    let snapshot = match endpoint {
        Endpoint::Before => journal.local_before.as_deref(),
        Endpoint::After if journal.local_before.is_some() => journal.worktree_after.as_deref(),
        Endpoint::After => None,
    };
    if let Some(snapshot) = snapshot {
        return checkout_matches
            && worktree_result_matches(repo, snapshot, Some(&format!("{snapshot}^2")));
    }
    if endpoint == Endpoint::After && journal.worktree_after.is_some() {
        return checkout_matches
            && worktree_result_matches(
                repo,
                journal.worktree_after.as_deref().unwrap(),
                journal
                    .refs
                    .iter()
                    .find(|reference| reference.name == format!("refs/heads/{branch}"))
                    .and_then(|reference| reference.after.as_deref()),
            );
    }
    let expected = if endpoint == Endpoint::After {
        journal
            .refs
            .iter()
            .find(|reference| reference.name == format!("refs/heads/{branch}"))
            .and_then(|reference| reference.after.as_deref())
    } else {
        Some(journal.worktree_before.as_str())
    };
    checkout_matches && expected.is_some_and(|expected| worktree_at(repo, expected))
}

fn worktree_result_matches(repo: &Path, expected: &str, expected_head: Option<&str>) -> bool {
    let Some(expected_head) = expected_head else {
        return false;
    };
    let expected_index = git::run(repo, ["rev-parse", &format!("{expected_head}^{{tree}}")]);
    let actual_index = index_tree(repo);
    matches!((expected_index, actual_index), (Ok(expected), Ok(actual)) if expected == actual)
        // The carried result is a stash-shaped commit whose tree is the
        // promised worktree endpoint. Compare it through a disposable index
        // so assume-unchanged and skip-worktree cannot hide edits made after
        // the interrupted apply.
        && deleted_paths_match(repo, expected_head, expected)
        && worktree_matches_without_index_flags(repo, expected)
}

/// The worktree-only comparison cannot see paths absent from its tree. A
/// recreated unstaged deletion is still tracked in the real index, so the
/// untracked collision check cannot protect it either.
fn deleted_paths_match(repo: &Path, index: &str, worktree: &str) -> bool {
    let Ok(deleted) = git::run_bytes(
        repo,
        [
            "diff-tree",
            "-r",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "--diff-filter=D",
            "-z",
            index,
            worktree,
            "--",
        ],
    ) else {
        return false;
    };
    if deleted.is_empty() {
        return true;
    }
    let Ok(listing) = git::run_bytes(repo, ["ls-tree", "-r", "--name-only", "-z", worktree]) else {
        return false;
    };
    let leaves: Vec<_> = listing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    deleted
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .all(|path| {
            // Directory -> file/symlink: the new leaf's ordinary comparison
            // verifies this replacement without following it to inspect children.
            if leaves.iter().any(|leaf| {
                path.strip_prefix(*leaf)
                    .is_some_and(|suffix| suffix.starts_with(b"/"))
            }) {
                return true;
            }
            let Some(name) = os_string_from_git_path(path) else {
                return false;
            };
            match fs::symlink_metadata(repo.join(name)) {
                // File -> directory: a directory is expected only when the
                // reviewed tree actually puts descendants underneath it.
                Ok(metadata) => {
                    metadata.is_dir()
                        && leaves.iter().any(|leaf| {
                            leaf.strip_prefix(path)
                                .is_some_and(|suffix| suffix.starts_with(b"/"))
                        })
                }
                Err(error) => matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ),
            }
        })
}

fn completion_safe(repo: &Path, journal: &Journal) -> bool {
    let Some(branch) = journal.checkout.as_deref() else {
        return true;
    };
    if ensure_known_worktree(repo, journal).is_err() {
        return false;
    }
    if collision::check_reset(repo, repo, &format!("refs/heads/{branch}")).is_err() {
        return false;
    }
    match journal.worktree_after.as_deref() {
        Some(result) => collision::check_restore(repo, repo, result).is_ok(),
        None => true,
    }
}

fn rollback_safe(repo: &Path, journal: &Journal, state: State) -> bool {
    let Some(branch) = journal.checkout.as_deref() else {
        return state == State::BeforeRefChange || ensure_checkout_not_moved(repo, journal).is_ok();
    };
    if ensure_checkout(repo, branch).is_err() {
        return false;
    }
    ensure_rollback_worktree(repo, journal).is_ok()
}

fn rollback_reset_safe(repo: &Path, journal: &Journal, branch: &str) -> Result<()> {
    let target = journal
        .refs
        .iter()
        .find(|reference| reference.name == format!("refs/heads/{branch}"))
        .and_then(|reference| reference.before.as_deref())
        .ok_or_else(|| {
            Error::Refused(format!(
                "apply recovery is blocked: the original checkout branch {branch} did not have \
                 a recorded ref to restore; no recovery action was performed."
            ))
        })?;
    collision::check_reset(repo, repo, target)
}

fn ensure_checkout_not_moved(repo: &Path, journal: &Journal) -> Result<()> {
    let Ok(current) = git::run(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]) else {
        return Ok(());
    };
    let name = format!("refs/heads/{current}");
    if journal.refs.iter().any(|reference| reference.name == name) {
        return Err(Error::Refused(format!(
            "apply recovery is blocked: the current checkout branch {current} is one of the \
             refs this operation would restore. Switch to the original checkout or detach HEAD; \
             no recovery action was performed."
        )));
    }
    Ok(())
}

fn update_worktree(repo: &Path, journal: &Journal) -> Result<()> {
    if let Some(result) = journal.worktree_after.as_deref() {
        collision::check_restore(repo, repo, result)?;
        if journal.local_before.is_some() {
            carry::restore_snapshot(repo, result)
        } else {
            carry::restore(repo, result)
        }
    } else {
        git::run(repo, ["reset", "--hard", "--quiet"])?;
        Ok(())
    }
}

fn state_of(repo: &Path) -> Result<BTreeMap<String, String>> {
    git::refs(repo, "refs/heads/", 0)
}

/// `write-tree` can update the index's cache-tree extension. Compare a copy
/// so inspection and refused recovery leave even the real index bytes alone.
fn index_tree(repo: &Path) -> Result<String> {
    let storage = git_dir(repo)?;
    let temporary = tempfile::tempdir_in(&storage).map_err(Error::io(&storage))?;
    let index = temporary.path().join("index");
    fs::copy(storage.join("index"), &index).map_err(Error::io(&index))?;
    git::run_with_clean_env(
        repo,
        ["write-tree"],
        &[("GIT_INDEX_FILE", index.into_os_string())],
    )
}

fn worktree_at(repo: &Path, expected_head: &str) -> bool {
    let expected_tree = git::run(repo, ["rev-parse", &format!("{expected_head}^{{tree}}")]);
    let index_tree = index_tree(repo);
    matches!((expected_tree, index_tree), (Ok(expected), Ok(index)) if expected == index)
        && worktree_matches_without_index_flags(repo, expected_head)
}

/// Compares the worktree with the expected commit through a disposable index.
///
/// `diff-files` deliberately trusts the index's assume-unchanged and
/// skip-worktree bits. That is useful for normal Git commands, but unsafe for
/// recovery: an edit made after a killed apply must never be hidden from the
/// guard that decides whether `reset --hard` is allowed to run. Loading the
/// expected tree into an alternate index gives Git the same path and mode
/// semantics without changing the user's real index or its flags.
fn worktree_matches_without_index_flags(repo: &Path, expected_head: &str) -> bool {
    let Ok(storage) = git_dir(repo) else {
        return false;
    };
    let Ok(template_dir) = tempfile::tempdir_in(&storage) else {
        return false;
    };
    let Ok(check_dir) = tempfile::tempdir_in(&storage) else {
        return false;
    };
    let template_index = template_dir.path().join("index");
    let populate_env = [
        ("GIT_INDEX_FILE", template_index.into_os_string()),
        (
            "GIT_WORK_TREE",
            check_dir.path().to_owned().into_os_string(),
        ),
    ];
    // Ask Git to build the expected index through the sparse-checkout-aware
    // worktree update path. Loading the full tree with `read-tree --reset`
    // makes intentionally absent paths look edited; `-u` applies the user's
    // actual sparse patterns and preserves those entries' skip-worktree bits.
    if git::run_with_clean_env(
        repo,
        ["read-tree", "-m", "-u", expected_head],
        &populate_env,
    )
    .is_err()
    {
        return false;
    }
    let Some(sparse_excluded) = sparse_excluded_paths(repo, &populate_env) else {
        return false;
    };

    // Build the comparison index separately without `-u`, so its stat cache
    // starts empty. The sparse template above supplies only the skip-worktree
    // decisions; no timestamp recorded for the disposable worktree can make
    // the comparison trust a real file without checking it.
    // Keep control files outside the populated worktree: tracked files may
    // legitimately be named `index` or `index.lock`.
    let check_index = template_dir.path().join("comparison-index");
    let check_env = [("GIT_INDEX_FILE", check_index.into_os_string())];
    if git::run_with_clean_env(repo, ["read-tree", "--reset", expected_head], &check_env).is_err() {
        return false;
    }
    if !sparse_excluded.is_empty() {
        let mut args = vec![
            OsString::from("update-index"),
            OsString::from("--skip-worktree"),
        ];
        args.push(OsString::from("--"));
        args.extend(sparse_excluded);
        if git::run_with_clean_env(repo, args, &check_env).is_err() {
            return false;
        }
    }
    git::run_with_clean_env(repo, ["update-index", "--refresh"], &check_env).is_ok()
        && git::run_with_clean_env(repo, ["diff-files", "--quiet"], &check_env).is_ok()
}

/// Finds sparse paths that are still absent from the real worktree. Sparse
/// checkout intentionally leaves those paths absent, but a user can
/// materialize one and edit it afterwards; materialized paths stay unflagged
/// in the comparison index so Git checks them like normal tracked paths.
fn sparse_excluded_paths(repo: &Path, env: &[(&str, OsString)]) -> Option<Vec<OsString>> {
    let listing = git::run_bytes_with_clean_env(repo, ["ls-files", "-t", "-z"], env).ok()?;
    let mut paths = Vec::new();
    for record in listing.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        if record.len() < 2 || record[1] != b' ' {
            return None;
        }
        if record[0] != b'S' {
            continue;
        }
        let path = os_string_from_git_path(&record[2..])?;
        if fs::symlink_metadata(repo.join(&path)).is_err() {
            paths.push(path);
        }
    }
    Some(paths)
}

#[cfg_attr(
    unix,
    allow(
        clippy::unnecessary_wraps,
        reason = "the Windows decoder can reject bytes, so this shared helper stays fallible"
    )
)]
fn os_string_from_git_path(path: &[u8]) -> Option<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Some(OsString::from_vec(path.to_owned()))
    }
    #[cfg(windows)]
    {
        String::from_utf8(path.to_owned()).ok().map(OsString::from)
    }
}

fn ensure_checkout(repo: &Path, expected: &str) -> Result<()> {
    let current = git::run(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_else(|_| "detached HEAD".to_owned());
    if current == expected {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "apply recovery is blocked: the operation belongs to branch {expected}, but this \
         worktree is on {current}. Switch back before recovering; no mutation was performed."
    )))
}

fn restore_refs(repo: &Path, journal: &Journal) -> Result<()> {
    let mut commands = String::new();
    for reference in &journal.refs {
        let (target, expected) = (reference.before.as_deref(), reference.after.as_deref());
        match (target, expected) {
            (Some(target), Some(expected)) => {
                let _ = write!(
                    commands,
                    "update {}\0{target}\0{expected}\0",
                    reference.name
                );
            }
            (Some(target), None) => {
                let _ = write!(commands, "create {}\0{target}\0", reference.name);
            }
            (None, Some(expected)) => {
                let _ = write!(commands, "delete {}\0{expected}\0", reference.name);
            }
            (None, None) => {}
        }
    }
    if !commands.is_empty() {
        git::run_with_stdin(
            repo,
            ["update-ref", "-m", "git-rehearse recovery", "--stdin", "-z"],
            Some(&commands),
        )?;
    }
    Ok(())
}

fn restore_previous(repo: &Path, journal: &Journal) -> Result<()> {
    if !undo_matches(repo, journal, State::RollingBack)? {
        return Err(Error::Refused(
            "the undo record changed during recovery; it was preserved and recovery is blocked"
                .to_owned(),
        ));
    }
    let undo_path = git_dir(repo)?.join(crate::undo::UNDO_FILE);
    if let Some(bytes) = &journal.previous_undo {
        return atomic_bytes(&undo_path, bytes);
    }
    if undo_path.exists() {
        fs::remove_file(&undo_path).map_err(Error::io(&undo_path))?;
    }
    Ok(())
}

fn remove_anchor(repo: &Path, journal: &Journal) -> Result<()> {
    if journal.anchor.is_empty() {
        return Ok(());
    }
    let anchored = git::refs(repo, &journal.anchor, 0)?;
    for name in anchored.keys() {
        git::run(repo, ["update-ref", "-d", name])?;
    }
    for (suffix, snapshot) in [
        ("original", &journal.local_before),
        ("carry", &journal.worktree_after),
    ] {
        if let Some(snapshot) = snapshot {
            git::run(
                repo,
                [
                    "update-ref",
                    "-d",
                    &format!("refs/rehearse/{}-{suffix}", journal.rehearsal),
                    snapshot,
                ],
            )?;
        }
    }
    Ok(())
}

fn remove_undo_for(repo: &Path, journal: &Journal) -> Result<()> {
    if journal.operation != Operation::Undo {
        return Ok(());
    }
    if !undo_matches(repo, journal, State::Complete)? {
        return Err(Error::Refused(
            "the undo record changed during recovery; it was preserved and recovery is blocked"
                .to_owned(),
        ));
    }
    let path = git_dir(repo)?.join(crate::undo::UNDO_FILE);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(&path)(error)),
    }
}

fn update_phase(path: &Path, lock: &Lock, phase: Phase) -> Result<()> {
    let mut journal = read(path)?;
    journal.phase = phase;
    write_phase(path, lock, &journal)
}

fn write_phase(path: &Path, lock: &Lock, journal: &Journal) -> Result<()> {
    write_owned(path, lock, journal, journal.phase.storage_failure_stage()).map_err(|error| {
        Error::Refused(format!(
            "could not durably advance apply journal at {}: {error}. Recovery is blocked; inspect \
         the real refs before taking any action.",
            path.display()
        ))
    })
}

fn read(path: &Path) -> Result<Journal> {
    let text = fs::read_to_string(path).map_err(Error::io(path))?;
    let stored: StoredJournal = serde_json::from_str(&text).map_err(|error| {
        Error::Refused(format!(
            "{} is a damaged apply journal: {error}; recovery is blocked and the file will \
             not be overwritten",
            path.display()
        ))
    })?;
    if checksum(path, &stored.journal)? != stored.checksum {
        return Err(Error::Refused(format!(
            "{} has a damaged apply journal checksum; recovery is blocked and the file will not be overwritten",
            path.display()
        )));
    }
    Ok(stored.journal)
}

fn checksum(path: &Path, journal: &Journal) -> Result<String> {
    let text = serde_json::to_string(journal)
        .map_err(|error| Error::Sandbox(format!("could not build apply journal: {error}")))?;
    // Hash only: no object is written. Git supplies a stable digest without
    // adding a second hashing implementation or treating this as authentication.
    git::run_with_stdin(
        path.parent().unwrap_or_else(|| Path::new(".")),
        ["hash-object", "--stdin"],
        Some(&text),
    )
}

fn encode(path: &Path, journal: &Journal) -> Result<Vec<u8>> {
    let stored = StoredJournal {
        journal: journal.clone(),
        checksum: checksum(path, journal)?,
    };
    serde_json::to_vec_pretty(&stored)
        .map_err(|error| Error::Sandbox(format!("could not build apply journal: {error}")))
}

fn write_atomic(path: &Path, journal: &Journal, failure_stage: &str) -> Result<()> {
    let text = encode(path, journal)?;
    atomic_bytes_at(path, &text, failure_stage)
}

fn write_owned(path: &Path, lock: &Lock, journal: &Journal, failure_stage: &str) -> Result<()> {
    if lock.journal_path != path {
        return Err(Error::Refused(format!(
            "apply recovery is blocked: journal ownership at {} changed; no repository mutation \
             was performed",
            path.display()
        )));
    }
    write_atomic(path, journal, failure_stage)
}

fn lock_path(journal_path: &Path) -> PathBuf {
    journal_path.with_extension(LOCK_SUFFIX)
}

fn pause_for_test(stage: &str) {
    crate::test_hooks::pause("GIT_REHEARSE_PAUSE_RECOVERY_AT", stage);
}

fn write_atomic_exclusive(path: &Path, journal: &Journal) -> Result<()> {
    let text = encode(path, journal)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::Builder::new()
        .prefix(".rehearse-apply-")
        .tempfile_in(parent)
        .map_err(Error::io(parent))?;
    file.write_all(&text).map_err(Error::io(file.path()))?;
    fail_storage_for_test("journal-publish", "write", path)?;
    file.as_file_mut()
        .sync_all()
        .map_err(Error::io(file.path()))?;
    fail_storage_for_test("journal-publish", "sync", path)?;
    // hard_link is the portable exclusive publication primitive available to
    // std: it either creates the final name atomically or leaves the existing
    // journal untouched when another Apply won the race.
    let published = fs::hard_link(file.path(), path);
    published.map_err(Error::io(path))?;
    sync_parent_directory(path)?;
    Ok(())
}

pub(crate) fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_bytes_at(path, bytes, "other")
}

fn atomic_bytes_at(path: &Path, bytes: &[u8], failure_stage: &str) -> Result<()> {
    // Include the process id so two independent CLI invocations cannot write
    // through the same temporary file while each believes its journal is
    // durable. The final rename remains the single atomic publication point.
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .map_err(Error::io(&tmp))?;
    file.write_all(bytes).map_err(Error::io(&tmp))?;
    fail_storage_for_test(failure_stage, "write", &tmp)?;
    file.sync_all().map_err(Error::io(&tmp))?;
    fail_storage_for_test(failure_stage, "sync", &tmp)?;
    fs::rename(&tmp, path).map_err(Error::io(path))?;
    sync_parent_directory(path)?;
    Ok(())
}

/// Flushes the directory entry created by a publication or removed by cleanup.
///
/// Windows requires `FILE_FLAG_BACKUP_SEMANTICS` to obtain a directory handle,
/// and `FlushFileBuffers` requires `GENERIC_WRITE`; `File::open` supplies
/// neither. Rust's `File::sync_all` calls `FlushFileBuffers` on that handle, so
/// keep the error visible instead of treating an unflushed directory as safe.
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // FILE_FLAG_BACKUP_SEMANTICS from WinBase.h. CreateFileW requires it
        // for directory handles; see the CreateFileW directory contract.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(Error::io(parent))?;
    }

    #[cfg(not(windows))]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(Error::io(parent))?;
    }

    Ok(())
}

/// Test-only filesystem-boundary fault injection. The child process sets
/// `GIT_REHEARSE_FAIL_RECOVERY_STORAGE_AT` to `<stage>-<operation>`; the
/// actual write has happened before a `sync` fault is returned, so recovery
/// tests exercise the durable-write error path without exhausting disk space.
fn fail_storage_for_test(stage: &str, operation: &str, path: &Path) -> Result<()> {
    let requested = format!("{stage}-{operation}");
    if std::env::var_os("GIT_REHEARSE_FAIL_RECOVERY_STORAGE_AT").as_deref()
        == Some(requested.as_ref())
    {
        return Err(Error::io(path)(std::io::Error::other(format!(
            "deterministic test storage failure while {operation} apply recovery journal"
        ))));
    }
    Ok(())
}

fn remove_journal(path: &Path) -> Result<()> {
    fs::remove_file(path).map_err(Error::io(path))?;
    sync_parent_directory(path)
}

fn git_dir(repo: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git::run(
        repo,
        ["rev-parse", "--absolute-git-dir"],
    )?))
}

fn blocked(inspection: &Inspection) -> Error {
    Error::Refused(format!(
        "{} recovery is required for rehearsal {}: observed {} after journal phase {:?}. \
         No repository mutation is allowed until it is resolved. Use `git rehearse recover \
         --complete {}` or `git rehearse recover --rollback {}`; if the state is ambiguous, \
         preserve the work and resolve it by hand.",
        inspection.operation.name(),
        inspection.rehearsal,
        match inspection.state {
            State::BeforeRefChange => "before_ref_change",
            State::AfterRefChange => "after_ref_change",
            State::Complete => "complete",
            State::RollingBack => "rolling_back",
            State::Ambiguous => "ambiguous",
        },
        inspection.phase,
        inspection.rehearsal,
        inspection.rehearsal
    ))
}

#[cfg(test)]
mod tests {
    use super::{Action, Journal, Operation, Phase, State, write_atomic_exclusive};
    #[cfg(windows)]
    use super::{atomic_bytes, remove_journal};
    use std::fs;

    #[test]
    fn phase_names_are_stable_for_json_and_recovery_messages() {
        assert_eq!(
            serde_json::to_string(&Phase::RefsApplied).unwrap(),
            "\"refs_applied\""
        );
        assert_eq!(
            serde_json::to_string(&State::BeforeRefChange).unwrap(),
            "\"before_ref_change\""
        );
        assert!(matches!(Action::Inspect, Action::Inspect));
    }

    #[test]
    fn journal_publication_never_replaces_an_existing_owner() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("rehearse-apply");
        fs::write(&path, b"the first apply owns this journal").expect("existing journal");
        let journal = Journal {
            schema: super::JOURNAL_SCHEMA,
            rehearsal: "second".to_owned(),
            origin: "/repo".to_owned(),
            checkout: None,
            refs: Vec::new(),
            anchor: String::new(),
            worktree_before: "abc".to_owned(),
            worktree_after: None,
            local_before: None,
            previous_undo: None,
            expected_undo: None,
            rollback_undo: None,
            operation: Operation::Apply,
            phase: Phase::Prepared,
        };

        assert!(write_atomic_exclusive(&path, &journal).is_err());
        assert_eq!(
            fs::read(&path).expect("journal remains"),
            b"the first apply owns this journal"
        );
    }

    #[cfg(windows)]
    #[test]
    fn journal_publication_and_removal_sync_the_parent_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("rehearse-apply");

        atomic_bytes(&path, b"durable journal").expect("journal publication");
        assert_eq!(
            fs::read(&path).expect("published journal"),
            b"durable journal"
        );

        remove_journal(&path).expect("journal removal");
        assert!(!path.exists(), "journal is removed");
    }
}
