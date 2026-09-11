//! Applying a rehearsal to a real repository.
//!
//! Design principle 2 is inviolable — **apply is a ref transplant, never a
//! re-run** — so most of these tests exist to prove that in ways a re-run
//! could not fake, and the rest exist to prove that a repository which moved
//! on is left alone.

mod support;

use git_rehearse::sandbox::{Plan, Sandbox};
use git_rehearse::{Error, apply, carry, execute, preflight, sandbox};
use std::process::{Command, Output};
use support::Fixture;

const NOW: u64 = 1_786_248_000;

fn plan_of(fixture: &Fixture, command: &[&str]) -> Plan {
    preflight::run(fixture.repo())
        .expect("the fixture passes preflight")
        .into_plan(command.iter().map(|arg| (*arg).to_owned()).collect())
}

/// Rehearses `command` and hands back the sandbox, ready to apply.
fn rehearse(fixture: &Fixture, command: &[&str]) -> Sandbox {
    let plan = plan_of(fixture, command);
    let mut sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    let outcome = execute::run(&sandbox.worktree(), &plan.command, None).expect("the command runs");
    sandbox.record(&outcome).expect("the outcome is recorded");
    sandbox
}

fn refusal(error: Error) -> String {
    match error {
        Error::Refused(message) => message,
        other => panic!("expected a refusal, got: {other:?}"),
    }
}

fn kept_merge(fixture: &Fixture) -> String {
    let output = fixture.rehearse(&["--json", "--keep", "merge", "--no-edit", "feature"]);
    assert_eq!(output.0, 0, "stdout={} stderr={}", output.1, output.2);
    serde_json::from_str::<serde_json::Value>(&output.1)
        .expect("the rehearsal report is JSON")
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("report has id")
        .to_owned()
}

fn kept(fixture: &Fixture, command: &[&str]) -> String {
    let mut args = vec!["--json", "--keep"];
    args.extend_from_slice(command);
    let output = fixture.rehearse(&args);
    assert_eq!(output.0, 0, "stdout={} stderr={}", output.1, output.2);
    serde_json::from_str::<serde_json::Value>(&output.1)
        .expect("the rehearsal report is JSON")
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("report has id")
        .to_owned()
}

fn abort_apply(fixture: &Fixture, id: &str, stage: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_REHEARSE_ABORT_APPLY_AT", stage)
        .args(["--json", "apply", id])
        .output()
        .expect("apply process runs")
}

fn fail_recovery_storage(fixture: &Fixture, id: &str, stage: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_REHEARSE_FAIL_RECOVERY_STORAGE_AT", stage)
        .args(["--json", "apply", id])
        .output()
        .expect("apply process runs")
}

fn abort_undo(fixture: &Fixture, stage: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_ABORT_UNDO_AT", stage)
        .args(["--json", "undo"])
        .output()
        .expect("undo process runs")
}

fn abort_rollback(fixture: &Fixture, id: &str, stage: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_REHEARSE_ABORT_RECOVERY_AT", stage)
        .args(["--json", "recover", "--rollback", id])
        .output()
        .expect("rollback runs")
}

fn assert_aborted(output: &Output, operation: &str) {
    #[cfg(windows)]
    assert!(
        !output.status.success(),
        "{operation} should be killed: {output:?}"
    );
    #[cfg(not(windows))]
    assert!(
        output.status.code().is_none(),
        "{operation} should be killed: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("git-rehearse test abort reached:"),
        "{operation} did not reach the forced-abort seam: {output:?}"
    );
}

#[test]
fn a_killed_apply_is_classified_and_rolled_back_without_repeating_it() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.refs();
    let id = kept_merge(&fixture);

    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    assert_eq!(status.0, 0, "stdout={} stderr={}", status.1, status.2);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["can_rollback"], true);
    assert_eq!(status["can_complete"], true);

    let rolled = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rolled.0, 0, "stdout={} stderr={}", rolled.1, rolled.2);
    assert_eq!(
        fixture.refs(),
        before,
        "rollback restored the exact ref state"
    );
    let no_journal = fixture.rehearse(&["--json", "recover"]);
    let no_journal: serde_json::Value = serde_json::from_str(&no_journal.1).expect("JSON");
    assert_eq!(no_journal["state"], "none");
}

#[test]
fn a_killed_apply_before_refs_can_only_be_rolled_back() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.refs();
    let id = kept_merge(&fixture);

    let killed = abort_apply(&fixture, &id, "after-journal");
    assert_aborted(&killed, "apply");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "before_ref_change");
    assert_eq!(status["can_complete"], false);
    assert_eq!(status["can_rollback"], true);

    let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(complete.0, 4, "before-ref state must refuse completion");
    let rolled = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rolled.0, 0, "rollback is safe before refs moved");
    assert_eq!(fixture.refs(), before);
}

fn recovery_refuses_flagged_external_edit(flag: &str) {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let after_main = fixture.git(&["rev-parse", "main"]);

    fixture.git(&["update-index", flag, "--", "file.txt"]);
    fixture.write("file.txt", "valuable external work\n");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "ambiguous");
    assert_eq!(status["can_rollback"], false);

    let journal = fixture.repo().join(".git/rehearse-apply");
    let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(complete.0, 4, "flagged external work blocks completion");
    assert!(journal.exists(), "blocked completion retains the journal");

    let rollback = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rollback.0, 4, "flagged external work blocks rollback");
    assert!(journal.exists(), "blocked rollback retains the journal");
    assert_eq!(
        fixture.git(&["rev-parse", "main"]),
        after_main,
        "blocked rollback leaves refs alone"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("external worktree"),
        "valuable external work\n"
    );
    let flag_state = fixture.git(&["ls-files", "-v", "--", "file.txt"]);
    assert!(
        (flag == "--assume-unchanged" && flag_state.starts_with('h'))
            || (flag == "--skip-worktree" && flag_state.starts_with('S')),
        "recovery must not mutate the index flag: {flag_state:?}"
    );
}

#[test]
fn recovery_refuses_assume_unchanged_external_edit() {
    recovery_refuses_flagged_external_edit("--assume-unchanged");
}

#[test]
fn recovery_refuses_skip_worktree_external_edit() {
    recovery_refuses_flagged_external_edit("--skip-worktree");
}

#[test]
fn an_interrupted_rollback_can_finish_restoring_the_worktree() {
    let fixture = Fixture::new();
    let original_refs = fixture.refs();
    let id = kept_merge(&fixture);
    let applied = abort_apply(&fixture, &id, "after-worktree-update");
    assert_aborted(&applied, "apply");

    let rollback = abort_rollback(&fixture, &id, "after-rollback-refs");
    assert_aborted(&rollback, "rollback");
    assert_eq!(
        fixture.git(&["rev-parse", "main"]),
        original_refs["refs/heads/main"]
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("applied worktree"),
        "three\n"
    );

    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("recovery JSON");
    assert_eq!(status["state"], "rolling_back");
    assert_eq!(status["can_complete"], false);
    assert_eq!(status["can_rollback"], true);
    let wrong_direction = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(wrong_direction.0, 4, "rollback intent cannot be discarded");

    fixture.write("file.txt", "external edit during rollback\n");
    let blocked = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(blocked.0, 4, "external edits still block resuming rollback");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("external work"),
        "external edit during rollback\n"
    );
    fixture.write("file.txt", "three\n");
    let finished = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(finished.0, 0, "rollback resumes: {}", finished.2);
    assert_eq!(fixture.refs(), original_refs);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("restored worktree"),
        "two\n"
    );
    assert!(fixture.git(&["status", "--porcelain"]).is_empty());
    let status = fixture.rehearse(&["--json", "recover"]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("recovery JSON");
    assert_eq!(status["state"], "none");
}

#[test]
fn rollback_resumes_after_recording_intent_or_restoring_the_worktree() {
    for stage in ["after-rollback-journal", "after-rollback-worktree"] {
        let fixture = Fixture::new();
        let original_refs = fixture.refs();
        let id = kept_merge(&fixture);
        let applied = abort_apply(&fixture, &id, "after-worktree-update");
        assert_aborted(&applied, "apply");
        assert_aborted(&abort_rollback(&fixture, &id, stage), "rollback");

        let status = fixture.rehearse(&["--json", "recover", &id]);
        let status: serde_json::Value = serde_json::from_str(&status.1).expect("recovery JSON");
        assert_eq!(status["state"], "rolling_back", "{stage}");
        assert_eq!(status["can_rollback"], true, "{stage}");
        let finished = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
        assert_eq!(finished.0, 0, "{stage}: {}", finished.2);
        assert_eq!(fixture.refs(), original_refs, "{stage}");
        assert_eq!(
            std::fs::read_to_string(fixture.repo().join("file.txt")).expect("restored worktree"),
            "two\n",
            "{stage}"
        );
        assert!(fixture.git(&["status", "--porcelain"]).is_empty());
    }
}

#[test]
fn a_blocked_rehearsal_does_not_write_snapshot_objects() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    fixture.write("file.txt", "work made after interruption\n");
    let objects = fixture.git(&["count-objects", "-v"]);

    let blocked = fixture.rehearse(&["--json", "--keep", "merge", "feature"]);

    assert_eq!(blocked.0, 4, "unresolved recovery blocks rehearsal");
    assert_eq!(fixture.git(&["count-objects", "-v"]), objects);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("external work"),
        "work made after interruption\n"
    );
}

#[test]
fn recovery_refuses_multiple_rehearsal_ids_without_mutating_refs() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let interrupted_refs = fixture.refs();

    let refused = fixture.rehearse(&["--json", "recover", "--rollback", &id, "another-id"]);

    assert_eq!(refused.0, 4, "extra IDs must be refused");
    assert_eq!(fixture.refs(), interrupted_refs);
    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
}

#[test]
fn recovery_refuses_conflicting_actions_without_changing_the_interrupted_state() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let interrupted_refs = fixture.refs();

    let conflict = fixture.rehearse(&["--json", "recover", "--complete", "--rollback", &id]);
    assert_eq!(conflict.0, 4, "conflicting actions must be refused");
    assert_eq!(fixture.refs(), interrupted_refs);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("interrupted worktree"),
        "two\n"
    );
    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
}

#[test]
fn recovery_completes_when_a_tracked_file_is_named_index() {
    let fixture = Fixture::new();
    fixture.commit_file(
        "index",
        "tracked index contents\n",
        "add ordinary index file",
    );
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let applied_refs = fixture.refs();

    let status = fixture.rehearse(&["--json", "recover", &id]);
    assert_eq!(status.0, 0, "inspection succeeds: {}", status.2);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["can_complete"], true);
    assert_eq!(status["can_rollback"], true);

    let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(complete.0, 0, "recovery completes: {}", complete.2);
    assert_eq!(fixture.refs(), applied_refs);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("index")).expect("tracked index file"),
        "tracked index contents\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("applied file"),
        "three\n"
    );
    assert!(fixture.git(&["status", "--porcelain"]).is_empty());
    let status = fixture.rehearse(&["--json", "recover"]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "none");
}

#[test]
fn recovery_rolls_back_when_a_tracked_file_is_named_index_lock() {
    let fixture = Fixture::new();
    fixture.commit_file(
        "index.lock",
        "tracked lock contents\n",
        "add ordinary lock file",
    );
    let original_refs = fixture.refs();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    assert_eq!(status.0, 0, "inspection succeeds: {}", status.2);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["can_complete"], true);
    assert_eq!(status["can_rollback"], true);

    let rollback = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rollback.0, 0, "recovery rolls back: {}", rollback.2);
    assert_eq!(fixture.refs(), original_refs);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("index.lock")).expect("tracked lock file"),
        "tracked lock contents\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("restored file"),
        "two\n"
    );
    assert!(fixture.git(&["status", "--porcelain"]).is_empty());
    let status = fixture.rehearse(&["--json", "recover"]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "none");
}

#[test]
fn recovery_completes_a_clean_sparse_checkout_after_ref_change() {
    let fixture = Fixture::new();
    fixture.commit_file("inside/a", "before\n", "add materialized path");
    fixture.commit_file("outside/b", "outside\n", "add sparse path");
    fixture.git(&["checkout", "-q", "-b", "sparse-feature"]);
    fixture.write("inside/a", "after\n");
    fixture.git(&["add", "--", "inside/a"]);
    fixture.git(&["commit", "-q", "-m", "change materialized path"]);
    fixture.git(&["checkout", "-q", "main"]);
    fixture.git(&["sparse-checkout", "init", "--cone"]);
    fixture.git(&["sparse-checkout", "set", "inside"]);

    let id = kept(&fixture, &["merge", "--no-edit", "sparse-feature"]);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    assert_eq!(status.0, 0, "inspection succeeds: {}", status.2);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["can_complete"], true);
    assert_eq!(status["can_rollback"], true);

    fixture.write("outside/b", "external work\n");
    let changed = fixture.rehearse(&["--json", "recover", &id]);
    let changed: serde_json::Value = serde_json::from_str(&changed.1).expect("status JSON");
    assert_eq!(changed["state"], "ambiguous");
    assert_eq!(changed["can_complete"], false);
    assert_eq!(changed["can_rollback"], false);
    std::fs::remove_file(fixture.repo().join("outside/b")).expect("clear external work");

    let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(complete.0, 0, "sparse recovery completes: {}", complete.2);
    assert_eq!(fixture.git(&["show", "HEAD:inside/a"]), "after");
    assert!(!fixture.repo().join("outside/b").exists());
    assert!(
        fixture
            .git(&["ls-files", "-t", "--", "outside/b"])
            .starts_with('S')
    );
    assert!(!fixture.repo().join(".git/rehearse-apply").exists());
}

#[test]
fn a_journal_publication_sync_failure_preserves_the_previous_undo_and_refs() {
    let fixture = Fixture::new();
    let first = kept(&fixture, &["--", "branch", "first-apply", "feature"]);
    let applied = fixture.rehearse(&["--json", "apply", &first]);
    assert_eq!(applied.0, 0, "first apply succeeds: {}", applied.2);
    let undo = fixture.repo().join(".git/rehearse-undo");
    let previous_undo = std::fs::read(&undo).expect("first apply writes undo");
    let before = fixture.refs();

    let second = kept(&fixture, &["--", "branch", "second-apply", "feature"]);
    let failed = fail_recovery_storage(&fixture, &second, "journal-publish-sync");
    assert_eq!(failed.status.code(), Some(4), "storage refusal: {failed:?}");
    let document: serde_json::Value = serde_json::from_slice(&failed.stdout).expect("failure JSON");
    assert_eq!(document["kind"], "refused");
    assert!(
        document["message"]
            .as_str()
            .is_some_and(|message| message.contains("could not durably publish apply journal")),
        "failure explains the blocked publication: {document}"
    );
    assert_eq!(fixture.refs(), before, "failed publication moves no refs");
    assert_eq!(std::fs::read(&undo).expect("undo remains"), previous_undo);
    assert!(
        !fixture.repo().join(".git/rehearse-apply").exists(),
        "a journal is not published when its sync fails"
    );
}

#[test]
fn apply_refuses_a_rehearsal_whose_anchor_is_already_owned() {
    let fixture = Fixture::new();
    let first = rehearse(&fixture, &["branch", "first-apply", "feature"]);
    apply::run(&first, NOW).expect("first apply succeeds");
    let before = fixture.refs();
    let undo_path = fixture.repo().join(".git/rehearse-undo");
    let undo = std::fs::read(&undo_path).unwrap();

    // Load metadata with a reused id, as an older allocator could produce.
    // Apply must check namespace ownership at its mutation boundary too.
    let second = rehearse(&fixture, &["branch", "second-apply", "feature"]);
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(second.root().join("meta.json")).unwrap()).unwrap();
    metadata["id"] = serde_json::json!(first.id());
    std::fs::write(
        second.root().join("meta.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    let second = sandbox::list(fixture.cache(), None)
        .expect("load rehearsals")
        .into_iter()
        .find(|found| found.root() == second.root())
        .expect("load colliding rehearsal");
    let refused = refusal(apply::run(&second, NOW).expect_err("occupied anchor is refused"));
    assert!(refused.contains("anchor"), "{refused}");
    assert_eq!(fixture.refs(), before);
    assert_eq!(std::fs::read(&undo_path).unwrap(), undo);
}

#[test]
fn a_later_journal_sync_failure_leaves_recoverable_refs_and_restores_previous_undo() {
    let fixture = Fixture::new();
    // Both rehearsals use the same timestamp, as on a fast CI runner. The
    // first sandbox is removed by Apply, but its recovery refs remain.
    let first = rehearse(&fixture, &["branch", "first-apply", "feature"]);
    let applied = fixture.rehearse(&["--json", "apply", first.id()]);
    assert_eq!(applied.0, 0, "first apply succeeds: {}", applied.2);
    let undo = fixture.repo().join(".git/rehearse-undo");
    let previous_undo = std::fs::read(&undo).expect("first apply writes undo");
    let before = fixture.refs();

    let second = rehearse(&fixture, &["branch", "second-apply", "feature"]);
    let failed = fail_recovery_storage(&fixture, second.id(), "refs-applied-sync");
    assert_eq!(failed.status.code(), Some(4), "storage refusal: {failed:?}");
    let document: serde_json::Value = serde_json::from_slice(&failed.stdout).expect("failure JSON");
    assert_eq!(document["kind"], "refused");
    assert!(
        document["message"]
            .as_str()
            .is_some_and(|message| message.contains("could not durably advance apply journal")),
        "failure explains the blocked phase update: {document}"
    );
    assert_ne!(fixture.refs(), before, "the real ref transaction landed");
    assert!(fixture.repo().join(".git/rehearse-apply").exists());

    let rolled = fixture.rehearse(&["--json", "recover", "--rollback", second.id()]);
    assert_eq!(rolled.0, 0, "rollback succeeds: {}", rolled.2);
    assert_eq!(fixture.refs(), before, "rollback restores exact refs");
    assert_eq!(
        std::fs::read(&undo).expect("previous undo restored"),
        previous_undo
    );
}

#[test]
fn a_killed_apply_after_worktree_update_is_already_complete() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let id = kept_merge(&fixture);

    let killed = abort_apply(&fixture, &id, "after-worktree-update");
    assert_aborted(&killed, "apply");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    assert_eq!(status.0, 0, "stdout={} stderr={}", status.1, status.2);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "complete");
    assert_eq!(status["can_rollback"], true);

    let completed = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(
        completed.0, 0,
        "stdout={} stderr={}",
        completed.1, completed.2
    );
    let status = fixture.rehearse(&["--json", "recover"]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("JSON");
    assert_eq!(status["state"], "none");
}

#[test]
fn recovery_refuses_when_the_interrupted_worktree_was_changed() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");

    fixture.write("file.txt", "new work\n");
    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "ambiguous");
    let rehearse = fixture.rehearse(&["--json", "--keep", "merge", "--no-edit", "feature"]);
    assert_eq!(rehearse.0, 4, "affected mutations stay blocked");
    let rollback = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rollback.0, 4, "changed worktree blocks recovery");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        "new work\n"
    );
}

#[test]
fn successful_recovery_json_reports_the_resulting_state() {
    for action in ["--complete", "--rollback"] {
        let fixture = Fixture::new();
        let id = kept_merge(&fixture);
        let killed = abort_apply(&fixture, &id, "after-ref-transaction");
        assert_aborted(&killed, "apply");

        let recovered = fixture.rehearse(&["--json", "recover", action, &id]);
        assert_eq!(recovered.0, 0, "{recovered:?}");
        let status: serde_json::Value = serde_json::from_str(&recovered.1).expect("recovery JSON");
        assert_eq!(status["state"], "none", "{action}");
        assert_eq!(status["can_complete"], false);
        assert_eq!(status["can_rollback"], false);
        assert!(status.get("journal").is_none());
        assert!(!fixture.repo().join(".git/rehearse-apply").exists());
    }
}

#[test]
fn valid_json_changes_to_the_journal_block_recovery_without_overwrite() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let path = fixture.repo().join(".git/rehearse-apply");
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    fixture.git(&["update-ref", "refs/safety/victim", "HEAD"]);
    let refs = fixture.refs();
    for (field, value) in [
        ("origin", serde_json::json!("a-different-repository")),
        ("anchor", serde_json::json!("refs/safety/")),
        ("previous_undo", serde_json::json!([98, 97, 100])),
        ("phase", serde_json::json!("rolling_back")),
        ("refs", serde_json::json!([])),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        let bytes = serde_json::to_vec(&changed).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        for arguments in [
            vec!["--json", "recover"],
            vec!["--json", "recover", "--rollback"],
        ] {
            let refused = fixture.rehearse(&arguments);
            assert_eq!(refused.0, 4, "{field}: {refused:?}");
            assert_eq!(fixture.refs(), refs, "{field}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{field}");
            assert_eq!(
                std::fs::read_to_string(fixture.repo().join("file.txt")).unwrap(),
                "two\n"
            );
        }
    }
}

#[test]
fn recovery_accepts_optional_journal_fields_but_refuses_a_copied_origin() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let path = fixture.repo().join(".git/rehearse-apply");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    journal["future_optional_field"] = serde_json::json!({"note": "ignored"});
    let bytes = serde_json::to_vec(&journal).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let accepted = fixture.rehearse(&["--json", "recover"]);
    assert_eq!(accepted.0, 0, "{accepted:?}");
    let status: serde_json::Value = serde_json::from_str(&accepted.1).unwrap();
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["operation"], "apply");

    let other = Fixture::new();
    let other_refs = other.refs();
    let copied = other.repo().join(".git/rehearse-apply");
    std::fs::write(&copied, &bytes).unwrap();
    let refused = other.rehearse(&["--json", "recover", "--rollback"]);
    assert_eq!(refused.0, 4, "{refused:?}");
    assert!(refused.2.contains("another repository"), "{refused:?}");
    assert_eq!(other.refs(), other_refs);
    assert_eq!(std::fs::read(&copied).unwrap(), bytes);
}

#[test]
fn a_damaged_journal_blocks_queries_and_apply_without_overwrite() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.refs();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-journal");
    assert_aborted(&killed, "apply");

    let journal = fixture.repo().join(".git/rehearse-apply");
    std::fs::write(&journal, b"partially written journal").expect("damage journal");
    let query = fixture.rehearse(&["--json", "recover", &id]);
    assert_eq!(query.0, 4, "damaged journal is a refusal");
    let document: serde_json::Value = serde_json::from_str(&query.1).expect("failure JSON");
    assert_eq!(document["kind"], "refused");
    assert_eq!(
        std::fs::read(&journal).expect("journal remains"),
        b"partially written journal"
    );

    let apply = fixture.rehearse(&["--json", "apply", &id]);
    assert_eq!(apply.0, 4, "blocked recovery prevents another apply");
    assert_eq!(fixture.refs(), before);
}

#[test]
fn pruning_keeps_a_sandbox_when_its_apply_journal_is_damaged() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-journal");
    assert_aborted(&killed, "apply");
    let journal = fixture.repo().join(".git/rehearse-apply");
    std::fs::write(&journal, b"damaged journal").expect("damage journal");
    let repo_id = git_rehearse::cache::repo_id(fixture.repo());
    let rehearsal =
        sandbox::find(fixture.cache(), &repo_id, Some(&id)).expect("sandbox remains findable");

    let removed =
        sandbox::prune(fixture.cache(), git_rehearse::now_unix() + 1, 0).expect("prune runs");
    assert!(
        removed.is_empty(),
        "damaged recovery state must be retained"
    );
    assert!(rehearsal.root().exists(), "sandbox is preserved for repair");
}

#[test]
fn a_killed_apply_before_journal_leaves_the_repository_untouched() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.refs();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "before-journal");
    assert_aborted(&killed, "apply");
    assert_eq!(fixture.refs(), before);
    assert!(!fixture.repo().join(".git/rehearse-apply").exists());
}

#[test]
fn recovery_refuses_an_untracked_file_that_the_incoming_ref_would_replace() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "new-file", "main"]);
    fixture.commit_file("incoming.txt", "committed\n", "add incoming file");
    fixture.git(&["checkout", "-q", "main"]);
    let id = kept(&fixture, &["merge", "--no-edit", "new-file"]);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");

    fixture.write("incoming.txt", "work made after the interruption\n");
    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(
        status["can_complete"], false,
        "the collision is actionable refusal"
    );

    let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(complete.0, 4, "recovery must refuse the overwrite");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("incoming.txt")).expect("user file"),
        "work made after the interruption\n"
    );
}

#[test]
fn rollback_refuses_new_untracked_work_before_inverse_reset() {
    let fixture = Fixture::new();
    fixture.commit_file("restore.txt", "original\n", "add restore path");
    fixture.git(&["checkout", "-q", "-b", "deleting", "main"]);
    fixture.git(&["rm", "-q", "restore.txt"]);
    fixture.git(&["commit", "-q", "-m", "delete restore path"]);
    fixture.git(&["checkout", "-q", "main"]);
    let id = kept(&fixture, &["merge", "--no-edit", "deleting"]);
    let killed = abort_apply(&fixture, &id, "after-worktree-update");
    assert_aborted(&killed, "apply");

    fixture.write("restore.txt", "new work\n");
    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "complete");
    assert_eq!(status["can_rollback"], false);

    let rollback = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rollback.0, 4, "inverse reset must refuse the collision");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("restore.txt")).expect("new work"),
        "new work\n"
    );
    assert_eq!(
        fixture.git(&["rev-parse", "main"]),
        fixture.git(&["rev-parse", "deleting"])
    );
}

#[test]
fn ref_only_rollback_refuses_when_the_newly_created_branch_is_checked_out() {
    let fixture = Fixture::new();
    let before = fixture.refs();
    let id = kept(&fixture, &["--", "branch", "new-branch", "feature"]);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");

    fixture.git(&["checkout", "-q", "new-branch"]);
    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "complete");
    assert_eq!(status["can_rollback"], false);

    let rollback = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(
        rollback.0, 4,
        "rollback must refuse deleting the checkout branch"
    );
    assert!(fixture.refs().contains_key("refs/heads/new-branch"));
    assert_eq!(fixture.git(&["branch", "--show-current"]), "new-branch");
    assert_ne!(fixture.refs(), before);
}

#[test]
fn a_live_completed_apply_keeps_its_journal_owner_until_exit() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "other", "main"]);
    fixture.commit_file("other.txt", "other\n", "other change");
    fixture.git(&["checkout", "-q", "main"]);
    let id = kept_merge(&fixture);
    let marker = fixture.scratch("apply-paused");
    std::fs::remove_dir(&marker).expect("marker starts absent");

    let mut first = Command::new(env!("CARGO_BIN_EXE_git-rehearse"));
    first
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env(
            "GIT_REHEARSE_PAUSE_APPLY_AT",
            format!("after-complete={}", marker.display()),
        )
        .args(["--json", "apply", &id]);
    let first = first.spawn().expect("first apply starts");
    // Prepared HEAD checks and endpoint validation may exceed five seconds
    // when the full real-Git suite runs concurrently.
    for _ in 0..6000 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(marker.exists(), "first apply reached its paused endpoint");

    let second = fixture.rehearse(&["--json", "--apply", "merge", "--no-edit", "other"]);
    assert_eq!(
        second.0, 4,
        "a live owner blocks a second affected mutation"
    );
    assert!(
        second.2.contains("live apply journal"),
        "owner refusal is explained: {}",
        second.2
    );

    std::fs::remove_file(&marker).expect("resume first apply");
    let first = first.wait_with_output().expect("first apply finishes");
    assert!(first.status.success(), "first apply succeeds: {first:?}");
    assert!(!fixture.repo().join(".git/rehearse-apply").exists());
}

#[test]
fn discard_keeps_recovery_ownership_through_actual_removal() {
    let fixture = Fixture::new();
    let discarded_id = kept_merge(&fixture);
    let applied_id = kept_merge(&fixture);
    let discarded = sandbox::find(
        fixture.cache(),
        &git_rehearse::cache::repo_id(fixture.repo()),
        Some(&discarded_id),
    )
    .expect("discarded rehearsal exists");
    let discarded_root = discarded.root().to_owned();
    let marker = fixture.scratch("discard-paused").join("marker");
    assert!(!marker.exists(), "marker starts absent");

    let mut first = Command::new(env!("CARGO_BIN_EXE_git-rehearse"));
    first
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env(
            "GIT_REHEARSE_PAUSE_SANDBOX_REMOVAL_AT",
            format!("after-removal={}", marker.display()),
        )
        .args(["--json", "discard", &discarded_id]);
    let first = first.spawn().expect("discard starts");
    for _ in 0..500 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(marker.exists(), "discard reached its post-removal pause");
    assert!(
        !discarded_root.exists(),
        "the real sandbox removal completed before the pause"
    );

    let blocked = fixture.rehearse(&["--json", "apply", &applied_id]);
    assert_eq!(blocked.0, 4, "apply is blocked during sandbox removal");
    assert!(
        blocked.2.contains("live apply journal"),
        "the lock refusal is explained: {}",
        blocked.2
    );

    std::fs::remove_file(&marker).expect("resume discard");
    let first = first.wait_with_output().expect("discard finishes");
    assert_eq!(first.status.code(), Some(0), "discard succeeds: {first:?}");
    let applied = fixture.rehearse(&["--json", "apply", &applied_id]);
    assert_eq!(applied.0, 0, "apply succeeds after discard: {applied:?}");
}

#[test]
fn prune_keeps_recovery_ownership_through_actual_removal() {
    let fixture = Fixture::new();
    let old_plan = fixture.plan(
        &["merge", "feature"],
        git_rehearse::sandbox::Checkout::Branch("main".to_owned()),
    );
    let old = sandbox::create(fixture.cache(), &old_plan, NOW).expect("old rehearsal");
    let old_root = old.root().to_owned();
    let applied_id = kept_merge(&fixture);
    let marker = fixture.scratch("prune-paused").join("marker");
    assert!(!marker.exists(), "marker starts absent");

    let mut first = Command::new(env!("CARGO_BIN_EXE_git-rehearse"));
    first
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env(
            "GIT_REHEARSE_PAUSE_SANDBOX_REMOVAL_AT",
            format!("after-removal={}", marker.display()),
        )
        .args(["--json", "list"]);
    let first = first.spawn().expect("list starts");
    for _ in 0..500 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(marker.exists(), "prune reached its post-removal pause");
    assert!(!old_root.exists(), "the real sandbox removal completed");

    let blocked = fixture.rehearse(&["--json", "apply", &applied_id]);
    assert_eq!(blocked.0, 4, "apply is blocked during sandbox pruning");
    assert!(
        blocked.2.contains("live apply journal"),
        "the lock refusal is explained: {}",
        blocked.2
    );

    std::fs::remove_file(&marker).expect("resume prune");
    let first = first.wait_with_output().expect("list finishes");
    assert_eq!(first.status.code(), Some(0), "list succeeds: {first:?}");
    let applied = fixture.rehearse(&["--json", "apply", &applied_id]);
    assert_eq!(applied.0, 0, "apply succeeds after prune: {applied:?}");
}

#[test]
fn recovery_comparison_stays_inside_the_repository() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let unavailable_temp = fixture.scratch("unavailable-system-temp").join("file");
    std::fs::write(&unavailable_temp, b"not a directory").expect("temp location sentinel");

    let output = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("TMPDIR", &unavailable_temp)
        .env("TMP", &unavailable_temp)
        .env("TEMP", &unavailable_temp)
        .args(["--json", "recover", &id])
        .output()
        .expect("recovery inspection runs");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).expect("recovery JSON");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["can_complete"], true);
    assert_eq!(
        std::fs::read(&unavailable_temp).unwrap(),
        b"not a directory"
    );
}

#[test]
fn recovery_lock_cannot_delete_a_replacement_journal() {
    let fixture = Fixture::new();
    // Create a completed journal (J0) by moving the checked-out branch and
    // stopping after its real worktree update.
    let initial = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let initial_id = initial.id().to_owned();
    let killed = abort_apply(&fixture, &initial_id, "after-worktree-update");
    assert_aborted(&killed, "initial apply");
    let journal_path = fixture.repo().join(".git/rehearse-apply");
    let completed_journal = std::fs::read(&journal_path).expect("completed journal");

    // Preflight legitimately reaps J0 while preparing later rehearsals.
    // Restore its exact persisted bytes to exercise ownership at that endpoint.
    let replacement = rehearse(&fixture, &["branch", "replacement", "main"]);
    let contender = rehearse(&fixture, &["branch", "contender", "main"]);
    std::fs::write(&journal_path, completed_journal).expect("restore completed endpoint");
    let marker = fixture.scratch("recovery-paused").join("marker");
    let mut first = Command::new(env!("CARGO_BIN_EXE_git-rehearse"));
    first
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env(
            "GIT_REHEARSE_PAUSE_RECOVERY_AT",
            format!("after-lock={}", marker.display()),
        )
        .args(["--json", "apply", contender.id()]);
    let first = first.spawn().expect("locked recovery starts");
    for _ in 0..500 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "first process paused while owning recovery lock"
    );

    // B cannot reclaim J0 or publish a replacement journal while A owns the
    // stable OS lock, so it is refused before any real ref mutation.
    let replacement_killed = abort_apply(&fixture, replacement.id(), "after-ref-transaction");
    assert!(
        replacement_killed.status.code() == Some(4),
        "replacement apply should be refused: {replacement_killed:?}"
    );
    assert!(fixture.repo().join(".git/rehearse-apply").exists());
    std::fs::remove_file(&marker).expect("resume locked recovery");
    let first = first.wait_with_output().expect("first apply finishes");
    assert_eq!(
        first.status.code(),
        Some(0),
        "first apply completes: {first:?}"
    );
    assert!(
        !fixture.repo().join(".git/rehearse-apply").exists(),
        "completed journal is cleaned by its owner"
    );
    assert_eq!(
        fixture.git(&["rev-parse", "contender"]),
        fixture.git(&["rev-parse", "main"]),
        "the owning apply's ref transaction landed"
    );
}

#[test]
fn rollback_restores_a_moved_branch_when_the_checkout_branch_did_not_move() {
    let fixture = Fixture::new();
    let before = fixture.refs();
    let id = kept(&fixture, &["--", "branch", "-f", "feature", "main"]);
    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");

    let rolled = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rolled.0, 0, "the inverse ref transaction is safe");
    assert_eq!(fixture.refs(), before);
}

#[test]
fn rollback_restores_the_previous_undo_record_after_an_interrupted_write() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let id = kept_merge(&fixture);
    let undo = fixture.repo().join(".git/rehearse-undo");
    std::fs::write(&undo, b"previous undo record\n").expect("old undo record");

    let killed = abort_apply(&fixture, &id, "after-ref-transaction");
    assert_aborted(&killed, "apply");
    let rolled = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
    assert_eq!(rolled.0, 0, "rollback is safe");
    assert_eq!(
        std::fs::read(&undo).expect("restored undo"),
        b"previous undo record\n"
    );
}

#[test]
fn recovery_preserves_an_externally_replaced_undo_record() {
    for interrupted_undo in [false, true] {
        let fixture = Fixture::new();
        let id = kept_merge(&fixture);
        let killed = if interrupted_undo {
            let applied = fixture.rehearse(&["--json", "apply", &id]);
            assert_eq!(applied.0, 0, "{applied:?}");
            abort_undo(&fixture, "after-worktree-update")
        } else {
            abort_apply(&fixture, &id, "after-ref-transaction")
        };
        assert_aborted(&killed, "interrupted operation");
        let path = fixture.repo().join(".git/rehearse-undo");
        std::fs::write(&path, b"valuable external undo record").unwrap();
        let refs = fixture.refs();
        let worktree = std::fs::read(fixture.repo().join("file.txt")).unwrap();
        for args in [
            vec!["--json", "recover", "--rollback"],
            vec!["--json", "recover", "--complete"],
            vec!["--json", "--keep", "merge", "feature"],
        ] {
            let refused = fixture.rehearse(&args);
            assert_eq!(refused.0, 4, "{args:?}: {refused:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"valuable external undo record"
            );
            assert_eq!(fixture.refs(), refs);
            assert_eq!(
                std::fs::read(fixture.repo().join("file.txt")).unwrap(),
                worktree
            );
            assert!(fixture.repo().join(".git/rehearse-apply").exists());
        }
    }
}

#[test]
fn undo_rollback_preserves_an_externally_removed_record() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    assert_eq!(fixture.rehearse(&["--json", "apply", &id]).0, 0);
    assert_aborted(&abort_undo(&fixture, "after-ref-transaction"), "undo");
    assert_aborted(
        &abort_rollback(&fixture, &id, "after-rollback-journal"),
        "rollback",
    );
    let path = fixture.repo().join(".git/rehearse-undo");
    let moved = fixture.scratch("external-undo").join("saved");
    std::fs::rename(&path, &moved).unwrap();
    let refs = fixture.refs();
    let refused = fixture.rehearse(&["--json", "recover", "--rollback"]);
    assert_eq!(refused.0, 4, "{refused:?}");
    assert!(!path.exists(), "external removal is preserved");
    assert!(moved.exists());
    assert_eq!(fixture.refs(), refs);
}

#[test]
fn undo_rollback_can_resume_after_the_completed_undo_consumed_its_record() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    assert_eq!(fixture.rehearse(&["--json", "apply", &id]).0, 0);
    let applied_refs = fixture.refs();
    assert_aborted(&abort_undo(&fixture, "after-undo-record-removal"), "undo");
    assert!(!fixture.repo().join(".git/rehearse-undo").exists());
    assert_aborted(
        &abort_rollback(&fixture, &id, "after-rollback-journal"),
        "rollback",
    );
    let completed = fixture.rehearse(&["--json", "recover", "--rollback"]);
    assert_eq!(completed.0, 0, "{completed:?}");
    assert_eq!(fixture.refs(), applied_refs);
    assert!(fixture.repo().join(".git/rehearse-undo").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).unwrap(),
        "three\n"
    );
}

#[test]
fn automatic_recovery_cleanup_consumes_an_already_completed_undo_record() {
    let fixture = Fixture::new();
    let id = kept_merge(&fixture);
    let applied = fixture.rehearse(&["--json", "apply", &id]);
    assert_eq!(applied.0, 0, "{applied:?}");
    let killed = abort_undo(&fixture, "after-worktree-update");
    assert_aborted(&killed, "undo");
    assert!(fixture.repo().join(".git/rehearse-undo").exists());

    let rehearsal = fixture.rehearse(&["--json", "--keep", "merge", "feature"]);
    assert_eq!(rehearsal.0, 0, "{rehearsal:?}");
    assert!(!fixture.repo().join(".git/rehearse-apply").exists());
    assert!(!fixture.repo().join(".git/rehearse-undo").exists());
    let repeated = fixture.rehearse(&["--json", "undo"]);
    assert_eq!(repeated.0, 4, "completed Undo cannot be repeated");
}

#[test]
fn a_killed_undo_is_journaled_and_can_be_completed() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let id = kept_merge(&fixture);
    let applied = fixture.rehearse(&["--json", "apply", &id]);
    assert_eq!(applied.0, 0, "apply succeeds: {}", applied.2);
    let before_undo = fixture.refs();

    let killed = abort_undo(&fixture, "after-ref-transaction");
    assert_aborted(&killed, "undo");
    let status = fixture.rehearse(&["--json", "recover"]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["operation"], "undo");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["can_complete"], true);

    let completed = fixture.rehearse(&["--json", "recover", "--complete"]);
    assert_eq!(completed.0, 0, "recovery completes undo: {}", completed.2);
    assert_ne!(
        fixture.refs(),
        before_undo,
        "undo restored the earlier refs"
    );
    assert!(!fixture.repo().join(".git/rehearse-undo").exists());
}

#[test]
fn carried_rollback_restores_original_staging_and_file_bytes() {
    for stage in [
        "after-journal",
        "after-ref-transaction",
        "after-reset-before-carried",
        "after-carry-files",
        "after-worktree-update",
    ] {
        let fixture = Fixture::new();
        fixture.commit_file("carried.txt", "base\n", "add carried path");
        fixture.write("carried.txt", "staged\n");
        fixture.git(&["add", "carried.txt"]);
        fixture.write("carried.txt", "unstaged\n");
        let refs = fixture.refs();
        let index = fixture.git(&["write-tree"]);
        let id = kept_merge(&fixture);
        assert_aborted(&abort_apply(&fixture, &id, stage), "apply");
        let rolled = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
        assert_eq!(rolled.0, 0, "{} {}", rolled.1, rolled.2);
        assert_eq!(fixture.refs(), refs);
        assert_eq!(fixture.git(&["write-tree"]), index);
        assert_eq!(
            std::fs::read(fixture.repo().join("carried.txt")).unwrap(),
            b"unstaged\n"
        );
    }
}

#[test]
fn recovery_finishes_a_carried_apply_after_reset() {
    let fixture = Fixture::new();
    fixture.commit_file("carried.txt", "base\n", "add carried path");
    fixture.write("carried.txt", "carried work\n");
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-reset-before-carried");
    assert_aborted(&killed, "apply");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "after_ref_change");
    assert_eq!(status["can_complete"], true);
    let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(complete.0, 0, "the reviewed carry can be finished");
    assert!(!fixture.repo().join(".git/rehearse-apply").exists());
    assert_eq!(
        std::fs::read(fixture.repo().join("carried.txt")).unwrap(),
        b"carried work\n"
    );
}

#[test]
fn carried_completion_uses_the_reviewed_conflict_resolution_and_index() {
    for stage in [
        "after-ref-transaction",
        "after-reset-before-carried",
        "after-carry-files",
    ] {
        let fixture = Fixture::new();
        fixture.write("file.txt", "local conflict\n");
        let preview = fixture.rehearse(&["--json", "--keep", "merge", "--no-edit", "feature"]);
        assert_eq!(preview.0, 2, "{} {}", preview.1, preview.2);
        let report: serde_json::Value = serde_json::from_str(&preview.1).unwrap();
        let id = report["id"].as_str().unwrap();
        let sandbox = std::path::Path::new(report["sandbox"].as_str().unwrap());
        std::fs::write(sandbox.join("file.txt"), b"reviewed staged resolution\n").unwrap();
        fixture.git_in(sandbox, &["add", "file.txt"]);
        std::fs::write(sandbox.join("file.txt"), b"reviewed final bytes\n").unwrap();
        let continued = fixture.rehearse(&["--json", "--keep", "continue", id]);
        assert_eq!(continued.0, 0, "{} {}", continued.1, continued.2);
        let expected_head = fixture.git_in(sandbox, &["rev-parse", "HEAD"]);
        let expected_index = fixture.git_in(sandbox, &["write-tree"]);
        assert_aborted(&abort_apply(&fixture, id, stage), "apply");
        let complete = fixture.rehearse(&["--json", "recover", "--complete", id]);
        assert_eq!(complete.0, 0, "{stage}: {} {}", complete.1, complete.2);
        assert_eq!(fixture.git(&["rev-parse", "HEAD"]), expected_head);
        assert_eq!(fixture.git(&["write-tree"]), expected_index);
        assert_eq!(
            std::fs::read(fixture.repo().join("file.txt")).unwrap(),
            b"reviewed final bytes\n"
        );
    }
}

#[test]
fn carried_rollback_can_resume_after_its_own_interruption() {
    for stage in [
        "after-rollback-journal",
        "after-rollback-refs",
        "after-carry-files",
        "after-rollback-worktree",
    ] {
        let fixture = Fixture::new();
        fixture.commit_file("local.txt", "base\n", "local base");
        fixture.write("local.txt", "staged\n");
        fixture.git(&["add", "local.txt"]);
        fixture.write("local.txt", "unstaged\n");
        let before = fixture.refs();
        let index = fixture.git(&["write-tree"]);
        let id = kept_merge(&fixture);
        assert_aborted(
            &abort_apply(&fixture, &id, "after-reset-before-carried"),
            "apply",
        );
        assert_aborted(&abort_rollback(&fixture, &id, stage), "rollback");
        let result = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
        assert_eq!(result.0, 0, "{stage}: {} {}", result.1, result.2);
        assert_eq!(fixture.refs(), before);
        assert_eq!(fixture.git(&["write-tree"]), index);
        assert_eq!(
            std::fs::read(fixture.repo().join("local.txt")).unwrap(),
            b"unstaged\n"
        );
    }
}

#[test]
fn carried_storage_failures_preserve_snapshots_and_allow_rollback() {
    for stage in [
        "worktree-after-sync",
        "refs-applied-sync",
        "worktree-updated-sync",
        "complete-sync",
    ] {
        let fixture = Fixture::new();
        fixture.commit_file("local.txt", "base\n", "local base");
        fixture.write("local.txt", "staged\n");
        fixture.git(&["add", "local.txt"]);
        fixture.write("local.txt", "unstaged\n");
        let before = fixture.refs();
        let index = fixture.git(&["write-tree"]);
        let id = kept_merge(&fixture);
        let failed = fail_recovery_storage(&fixture, &id, stage);
        assert_eq!(failed.status.code(), Some(4), "{stage}: {failed:?}");
        if stage == "worktree-after-sync" || stage == "refs-applied-sync" {
            let discarded = fixture.rehearse(&["--json", "discard", &id]);
            assert_eq!(discarded.0, 4, "recovery retains the sandbox");
        }
        fixture.git(&["reflog", "expire", "--expire=now", "--all"]);
        fixture.git(&["gc", "--prune=now"]);
        let rolled = fixture.rehearse(&["--json", "recover", "--rollback", &id]);
        assert_eq!(rolled.0, 0, "{stage}: {} {}", rolled.1, rolled.2);
        assert_eq!(fixture.refs(), before);
        assert_eq!(fixture.git(&["write-tree"]), index);
        assert_eq!(
            std::fs::read(fixture.repo().join("local.txt")).unwrap(),
            b"unstaged\n"
        );
    }
}

#[test]
fn carried_recovery_preserves_external_edits_and_staging() {
    for restage in [false, true] {
        let fixture = Fixture::new();
        fixture.commit_file("local.txt", "base\n", "local base");
        fixture.write("local.txt", "carried\n");
        let id = kept_merge(&fixture);
        assert_aborted(
            &abort_apply(&fixture, &id, "after-ref-transaction"),
            "apply",
        );
        if restage {
            fixture.git(&["add", "local.txt"]);
        } else {
            fixture.write("local.txt", "external\n");
        }
        let refs = fixture.refs();
        let index = std::fs::read(fixture.repo().join(".git/index")).unwrap();
        let bytes = std::fs::read(fixture.repo().join("local.txt")).unwrap();
        for action in ["--complete", "--rollback"] {
            let result = fixture.rehearse(&["--json", "recover", action, &id]);
            assert_eq!(result.0, 4, "{action}: {} {}", result.1, result.2);
            assert_eq!(fixture.refs(), refs);
            assert_eq!(
                std::fs::read(fixture.repo().join(".git/index")).unwrap(),
                index
            );
            assert_eq!(
                std::fs::read(fixture.repo().join("local.txt")).unwrap(),
                bytes
            );
        }
    }
}

#[test]
fn carried_recovery_preserves_external_recreation_of_a_deleted_tracked_path() {
    let fixture = Fixture::new();
    fixture.commit_file("local.txt", "base\n", "local base");
    std::fs::remove_file(fixture.repo().join("local.txt")).unwrap();
    let id = kept_merge(&fixture);
    assert_aborted(
        &abort_apply(&fixture, &id, "after-ref-transaction"),
        "apply",
    );
    fixture.write("local.txt", "external recreated file\n");
    let refs = fixture.refs();
    let index = std::fs::read(fixture.repo().join(".git/index")).unwrap();
    let result = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(result.0, 4, "{} {}", result.1, result.2);
    assert_eq!(fixture.refs(), refs);
    assert_eq!(
        std::fs::read(fixture.repo().join(".git/index")).unwrap(),
        index
    );
    assert_eq!(
        std::fs::read(fixture.repo().join("local.txt")).unwrap(),
        b"external recreated file\n"
    );
}

#[test]
fn carried_recovery_refuses_untracked_and_ignored_collisions() {
    for ignored in [false, true] {
        let fixture = Fixture::new();
        fixture.commit_file("local.txt", "base\n", "local base");
        fixture.write("added.txt", "tracked local addition\n");
        fixture.git(&["add", "added.txt"]);
        let id = kept_merge(&fixture);
        assert_aborted(
            &abort_apply(&fixture, &id, "after-reset-before-carried"),
            "apply",
        );
        fixture.write("added.txt", "external untracked work\n");
        if ignored {
            std::fs::write(fixture.repo().join(".git/info/exclude"), "added.txt\n").unwrap();
        }
        let refs = fixture.refs();
        let index = fixture.git(&["write-tree"]);
        for action in ["--complete", "--rollback"] {
            let result = fixture.rehearse(&["--json", "recover", action, &id]);
            assert_eq!(result.0, 4, "{action}: {} {}", result.1, result.2);
            assert_eq!(fixture.refs(), refs);
            assert_eq!(fixture.git(&["write-tree"]), index);
            assert_eq!(
                std::fs::read(fixture.repo().join("added.txt")).unwrap(),
                b"external untracked work\n"
            );
        }
    }
}

#[test]
fn recovery_recognizes_a_carried_result_after_worktree_restore() {
    let fixture = Fixture::new();
    fixture.commit_file("carried.txt", "base\n", "add carried path");
    fixture.write("carried.txt", "carried work\n");
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-worktree-update");
    assert_aborted(&killed, "apply");

    let status = fixture.rehearse(&["--json", "recover", &id]);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "complete");
    let completed = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(completed.0, 0, "completed carry can be acknowledged");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("carried.txt")).expect("carried file"),
        "carried work\n"
    );
}

#[test]
fn recovery_refuses_carried_result_when_a_flagged_tracked_file_was_edited() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let fixture = Fixture::new();
        fixture.commit_file("other.txt", "base other\n", "add other path");
        fixture.write("other.txt", "carried work\n");
        let id = kept_merge(&fixture);
        let killed = abort_apply(&fixture, &id, "after-worktree-update");
        assert_aborted(&killed, "apply");

        fixture.git(&["update-index", flag, "--", "file.txt"]);
        fixture.write("file.txt", "external work\n");

        let status = fixture.rehearse(&["--json", "recover", &id]);
        assert_eq!(status.0, 0, "inspection succeeds: {}", status.2);
        let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
        assert_eq!(status["state"], "ambiguous", "flag: {flag}");
        assert_eq!(status["can_complete"], false, "flag: {flag}");

        let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
        assert_eq!(
            complete.0, 4,
            "flagged external work must remain blocked: {flag}"
        );
        assert!(
            fixture.repo().join(".git/rehearse-apply").exists(),
            "blocked completion retains the journal: {flag}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.repo().join("file.txt")).expect("external worktree"),
            "external work\n",
            "blocked completion preserves external work: {flag}"
        );
    }
}

#[test]
fn recovery_refuses_a_carried_result_when_the_index_was_restaged() {
    let fixture = Fixture::new();
    fixture.commit_file("carried.txt", "base\n", "add carried path");
    fixture.commit_file("also-carried.txt", "base\n", "add second carried path");
    fixture.write("carried.txt", "carried work\n");
    fixture.write("also-carried.txt", "also carried work\n");
    let id = kept_merge(&fixture);
    let killed = abort_apply(&fixture, &id, "after-worktree-update");
    assert_aborted(&killed, "apply");

    // Keep the carried worktree bytes intact while changing only the index.
    // This is the exact state recovery must refuse: the carried endpoint is
    // known, but its index no longer proves the endpoint that was rehearsed.
    fixture.git(&["add", "--", "carried.txt"]);
    let status = fixture.rehearse(&["--json", "recover", &id]);
    assert_eq!(status.0, 0, "inspection succeeds: {}", status.2);
    let status: serde_json::Value = serde_json::from_str(&status.1).expect("status JSON");
    assert_eq!(status["state"], "ambiguous");
    assert_eq!(status["can_complete"], false);

    let complete = fixture.rehearse(&["--json", "recover", "--complete", &id]);
    assert_eq!(complete.0, 4, "restaged carried work must remain blocked");
    assert!(fixture.repo().join(".git/rehearse-apply").exists());
}

#[test]
fn the_repository_ends_up_at_exactly_the_rehearsed_commits() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let rehearsed = fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]);

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        fixture.git(&["rev-parse", "main"]),
        rehearsed,
        "the same commit id, not an equivalent one"
    );
    assert_eq!(applied.moved.len(), 2, "main and HEAD: {:?}", applied.moved);
}

#[test]
fn apply_transplants_and_cannot_be_re_running_the_command() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let worktree = sandbox.worktree();

    // Give the rehearsed commit a message that re-running `git merge` could
    // never produce. If apply re-ran the command, this would be gone.
    fixture.git_in(
        &worktree,
        &["commit", "--amend", "-m", "REHEARSED-NOT-RERUN"],
    );
    let rehearsed = fixture.git_in(&worktree, &["rev-parse", "main"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(fixture.git(&["rev-parse", "main"]), rehearsed);
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%s", "main"]),
        "REHEARSED-NOT-RERUN",
        "the applied commit is the object that was inspected, byte for byte"
    );
}

#[test]
fn the_worktree_is_brought_to_the_rehearsed_content() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let mut sandbox = rehearse(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();
    // Finish the rebase the fixture's conflict stopped.
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);
    sandbox
        .record(&execute::Outcome::Clean)
        .expect("the completed outcome is recorded");

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(applied.reset.as_deref(), Some("feature"));
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "resolved\n",
        "the checked-out branch was rewritten, so the worktree follows it"
    );
    assert_eq!(
        fixture.git(&["status", "--porcelain"]),
        "",
        "and the index agrees with it"
    );
}

#[test]
fn a_commit_made_since_the_rehearsal_stops_everything() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    // Someone — or the user in another terminal — commits meanwhile.
    fixture.commit_file("later.txt", "later\n", "five");
    let before = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("refs/heads/main is now"), "{message}");
    assert!(message.contains("Rehearse again"), "{message}");
    assert_eq!(
        fixture.refs(),
        before,
        "a refused apply must leave the repository exactly as it found it"
    );
}

#[test]
fn switching_branches_between_rehearsing_and_applying_is_refused() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    fixture.git(&["checkout", "feature"]);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("you rehearsed on main"), "{message}");
    assert!(message.contains("now on feature"), "{message}");
}

#[test]
fn uncommitted_work_is_never_destroyed_by_an_apply() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let mut sandbox = rehearse(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);
    sandbox
        .record(&execute::Outcome::Clean)
        .expect("the completed outcome is recorded");
    // The user starts editing while reading the report.
    fixture.write("file.txt", "work in progress\n");
    let before = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("reset --hard"), "{message}");
    assert!(message.contains("commit or stash them"), "{message}");
    assert!(
        message.contains("not there when you rehearsed"),
        "the rehearsal carried nothing, so this edit appeared afterwards and is nobody's \
         business but the user's: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "work in progress\n",
        "the edit is still there"
    );
    assert_eq!(fixture.refs(), before, "and nothing moved");
}

#[test]
fn the_undo_record_is_written_with_both_sides_of_every_move() {
    // Both sides, because undo has to state the value it expects to replace or
    // it cannot have the guarantee apply has — see `undo.rs`, which owns the
    // format and proves the restore itself works.
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.git(&["rev-parse", "main"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    let record = std::fs::read_to_string(&applied.undo).expect("the undo file exists");
    let after = fixture.git(&["rev-parse", "main"]);
    assert!(
        record.contains(&format!("refs/heads/main {before} {after}")),
        "{record}"
    );
    assert!(record.contains(sandbox.id()), "{record}");
    assert!(record.contains("version 1"), "{record}");
    assert!(
        record.contains("git update-ref"),
        "the record should say how to use it: {record}"
    );

    // Written before anything moved, so a crash between here and there still
    // leaves the way back on disk.
    assert_ne!(after, before);
}

#[test]
fn the_reflog_says_where_the_new_commits_came_from() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    let reflog = fixture.git(&["reflog", "show", "--format=%gs", "main"]);
    assert!(
        reflog.contains(&format!("git-rehearse apply {}", sandbox.id())),
        "someone looking for what happened to their branch should find it: {reflog}"
    );
}

#[test]
fn the_rehearsed_commits_are_anchored_against_garbage_collection() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let rehearsed = fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]);

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        fixture.git(&["rev-parse", &format!("{}main", applied.anchor)]),
        rehearsed,
        "the transplanted commits keep a ref of their own"
    );
    // And they survive the sandbox being thrown away, which is the point.
    sandbox.discard().expect("discard");
    fixture.git(&["cat-file", "-e", &rehearsed]);
}

#[test]
fn a_branch_the_rehearsal_deletes_is_deleted() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "-D", "feature"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert!(
        !fixture.refs().contains_key("refs/heads/feature"),
        "{:?}",
        fixture.refs()
    );
}

#[test]
fn a_branch_the_rehearsal_creates_is_created() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "spike", "feature"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        fixture.git(&["rev-parse", "spike"]),
        fixture.git(&["rev-parse", "feature"])
    );
}

#[test]
fn a_branch_that_appeared_meanwhile_is_not_overwritten() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "spike", "feature"]);
    // The same name, created in the real repository in the meantime.
    fixture.git(&["branch", "spike", "main"]);
    let theirs = fixture.git(&["rev-parse", "spike"]);

    let error = apply::run(&sandbox, NOW).expect_err("refused");

    assert!(
        matches!(error, Error::Git { .. }),
        "git's own transaction refuses it: {error:?}"
    );
    assert_eq!(
        fixture.git(&["rev-parse", "spike"]),
        theirs,
        "their branch is untouched"
    );
}

#[test]
fn nothing_is_applied_when_any_part_of_the_transaction_would_fail() {
    let fixture = Fixture::new();
    // Two branches move: one is fine, one is not.
    fixture.git(&["branch", "second", "feature"]);
    let sandbox = rehearse(&fixture, &["branch", "-f", "second", "main"]);
    let worktree = sandbox.worktree();
    fixture.git_in(&worktree, &["branch", "-f", "feature", "main"]);
    // `feature` moves in the real repository after the rehearsal.
    fixture.git(&["branch", "-f", "feature", "main~1"]);
    let before = fixture.refs();

    apply::run(&sandbox, NOW).expect_err("refused");

    assert_eq!(
        fixture.refs(),
        before,
        "the good half of the batch must not land on its own"
    );
}

#[test]
fn a_rehearsal_that_moved_nothing_says_so_instead_of_pretending() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["merge", "main"]);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("moved no refs"), "{message}");
    assert!(message.contains("discard"), "{message}");
}

#[test]
fn applying_twice_is_refused_rather_than_repeated() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    apply::run(&sandbox, NOW).expect("the first apply succeeds");
    let after_first = fixture.refs();
    let message = refusal(apply::run(&sandbox, NOW).expect_err("the second is refused"));

    // The repository has changed — by the first apply. The check does not care
    // who moved the ref, which is exactly right.
    assert!(message.contains("has changed since rehearsal"), "{message}");
    assert_eq!(fixture.refs(), after_first);
}

#[test]
fn an_untracked_file_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "collision-feature", "main"]);
    fixture.commit_file("collision.txt", "feature content\n", "add collision");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "collision-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        fixture.refs(),
        before_refs,
        "no refs or fetch anchors moved"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
    assert!(!undo.exists(), "a refused apply has no undo record");
}

#[test]
fn identical_contents_do_not_allow_an_untracked_file_to_become_tracked() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "identical-feature", "main"]);
    fixture.commit_file("collision.txt", "same contents\n", "add identical file");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "same contents\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "identical-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);
    std::fs::write(&undo, "previous undo record\n").expect("existing undo record");

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        fixture.refs(),
        before_refs,
        "refs and fetch anchors are unchanged"
    );
    assert_eq!(fixture.git(&["ls-files", "--", "collision.txt"]), "");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "same contents\n"
    );
    assert_eq!(
        std::fs::read_to_string(undo).expect("undo"),
        "previous undo record\n"
    );
}

#[test]
fn a_non_colliding_untracked_file_survives_apply() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "feature-file", "main"]);
    fixture.commit_file("tracked-by-rehearsal.txt", "feature content\n", "add file");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("scratch.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature-file"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("scratch.txt")).expect("untracked file"),
        "user content\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("tracked-by-rehearsal.txt"))
            .expect("rehearsed file"),
        "feature content\n"
    );
}

#[cfg(unix)]
#[test]
fn identical_targets_do_not_allow_an_untracked_symlink_to_become_tracked() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "identical-link-feature", "main"]);
    let path = fixture.repo().join("collision-link");
    std::os::unix::fs::symlink("file.txt", &path).expect("target symlink");
    fixture.git(&["add", "collision-link"]);
    fixture.git(&["commit", "-m", "add identical symlink"]);
    fixture.git(&["checkout", "-q", "main"]);
    std::os::unix::fs::symlink("file.txt", &path).expect("untracked symlink");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "identical-link-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_link(path).expect("symlink"),
        std::path::Path::new("file.txt")
    );
    assert_eq!(fixture.git(&["ls-files", "--", "collision-link"]), "");
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn identical_contents_do_not_allow_a_case_equivalent_untracked_file_to_become_tracked() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "identical-case-feature", "main"]);
    fixture.commit_file(
        "Collision.txt",
        "same contents\n",
        "add case-equivalent file",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "same contents\n");
    if !fixture.repo().join("Collision.txt").exists() {
        return; // No filesystem alias on a case-sensitive volume.
    }
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "identical-case-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "same contents\n"
    );
}

#[test]
fn an_untracked_nested_repository_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "nested-feature", "main"]);
    fixture.commit_file(
        "vendor/important.txt",
        "feature content\n",
        "add nested file",
    );
    fixture.git(&["checkout", "-q", "main"]);

    let vendor = fixture.repo().join("vendor");
    std::fs::create_dir_all(&vendor).expect("nested repository directory");
    fixture.git_in(&vendor, &["init", "-q"]);
    fixture.git_in(&vendor, &["config", "user.name", "Fixture"]);
    fixture.git_in(
        &vendor,
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(vendor.join("important.txt"), "user content\n").expect("untracked nested file");

    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "nested-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(vendor.join("important.txt")).expect("nested file"),
        "user content\n"
    );
    assert!(!undo.exists(), "a refused apply has no undo record");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn distinct_non_utf8_untracked_names_do_not_collide() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "non-utf8-feature", "main"]);
    let tracked = OsString::from_vec(b"tracked-\x80.txt".to_vec());
    std::fs::write(fixture.repo().join(&tracked), "feature content\n").expect("tracked file");
    let status = Command::new("git")
        .arg("-C")
        .arg(fixture.repo())
        .args(["add", "--"])
        .arg(&tracked)
        .status()
        .expect("git add runs");
    assert!(status.success(), "git add failed");
    fixture.git(&["commit", "-m", "add non-UTF-8 file"]);
    fixture.git(&["checkout", "-q", "main"]);

    let untracked = OsString::from_vec(b"tracked-\x81.txt".to_vec());
    std::fs::write(fixture.repo().join(&untracked), "user content\n").expect("untracked file");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "non-utf8-feature"]);

    apply::run(&sandbox, NOW).expect("distinct names do not collide");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join(&untracked)).expect("untracked file"),
        "user content\n"
    );
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn case_equivalent_untracked_names_are_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "case-feature", "main"]);
    fixture.commit_file("Collision.txt", "feature content\n", "add collision");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "case-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
}

#[test]
fn an_ignored_untracked_file_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.commit_file(".gitignore", "ignored.txt\n", "ignore a generated file");
    fixture.git(&["checkout", "-q", "-b", "ignored-feature", "main"]);
    fixture.write("ignored.txt", "feature content\n");
    fixture.git(&["add", "-f", "ignored.txt"]);
    fixture.git(&["commit", "-m", "add ignored file"]);
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("ignored.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "ignored-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("ignored.txt")).expect("ignored file"),
        "user content\n"
    );
}

#[test]
fn an_untracked_gitignore_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "gitignore-feature", "main"]);
    fixture.commit_file(".gitignore", "ignored.txt\n", "add ignore rules");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write(".gitignore", "user-pattern\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "gitignore-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join(".gitignore")).expect("ignore file"),
        "user-pattern\n"
    );
}

#[test]
fn an_empty_untracked_directory_that_would_become_a_file_is_not_replaced() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "empty-dir-feature", "main"]);
    fixture.commit_file("empty-dir", "feature content\n", "replace empty directory");
    fixture.git(&["checkout", "-q", "main"]);
    std::fs::create_dir(fixture.repo().join("empty-dir")).expect("empty untracked directory");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "empty-dir-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert!(fixture.repo().join("empty-dir").is_dir());
}

#[cfg(unix)]
#[test]
fn an_empty_untracked_directory_is_not_replaced_by_a_symlink_to_a_directory() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "directory-link-feature", "main"]);
    let path = fixture.repo().join("empty-dir");
    std::os::unix::fs::symlink(".", &path).expect("target symlink");
    fixture.git(&["add", "empty-dir"]);
    fixture.git(&["commit", "-m", "add directory symlink"]);
    fixture.git(&["checkout", "-q", "main"]);
    std::fs::create_dir(&path).expect("empty untracked directory");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "directory-link-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);
    std::fs::write(&undo, "previous undo record\n").expect("existing undo record");

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        fixture.refs(),
        before_refs,
        "refs and fetch anchors are unchanged"
    );
    let metadata = std::fs::symlink_metadata(&path).expect("original directory");
    assert!(
        metadata.file_type().is_dir(),
        "must remain a directory, not a symlink"
    );
    assert_eq!(std::fs::read_dir(path).expect("directory").count(), 0);
    assert_eq!(
        std::fs::read_to_string(undo).expect("undo"),
        "previous undo record\n"
    );
}

#[cfg(unix)]
#[test]
fn collision_probe_ignores_command_scoped_hooks_and_excludes_config() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "config-feature", "main"]);
    fixture.commit_file("collision.txt", "feature content\n", "add collision");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "config-feature"]);

    let hooks = fixture.scratch("command-hooks");
    let marker = fixture.base().join("command-hook-ran");
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nprintf hook > '{}'\n", marker.display()),
    )
    .expect("hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("hook metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).expect("hook executable");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", &hooks)
        .env("GIT_CONFIG_KEY_1", "core.excludesFile")
        .env("GIT_CONFIG_VALUE_1", fixture.repo().join("collision.txt"))
        .args(["apply", sandbox.id()])
        .output()
        .expect("apply runs");

    assert!(
        !output.status.success(),
        "stdout={} stderr={} file={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read(fixture.repo().join("collision.txt"))
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("untracked"));
    assert!(!marker.exists(), "probe ran a command-scoped hook");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
}

#[test]
fn sparse_checkout_preserves_an_untracked_path_outside_the_sparse_cone() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "sparse-feature", "main"]);
    fixture.commit_file(
        "outside/tracked.txt",
        "feature content\n",
        "add outside sparse cone",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.git(&["sparse-checkout", "init", "--cone"]);
    fixture.git(&["sparse-checkout", "set", "--skip-checks", "file.txt"]);
    fixture.write("outside/untracked.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "sparse-feature"]);

    apply::run(&sandbox, NOW).expect("sparse-excluded untracked path is preserved");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("outside/untracked.txt"))
            .expect("untracked file"),
        "user content\n"
    );
}

#[test]
fn collision_probe_supports_split_indexes() {
    let fixture = Fixture::new();
    fixture.git(&["config", "core.splitIndex", "true"]);
    fixture.git(&["update-index", "--split-index"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    apply::run(&sandbox, NOW).expect("split index is supported");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("carried file"),
        "three\n"
    );
}

#[test]
fn a_sparse_reset_leaves_an_identical_untracked_file_at_a_skipped_target_path_alone() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "sparse-identical-feature", "main"]);
    fixture.commit_file(
        "outside/collision.txt",
        "same contents\n",
        "add excluded file",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.git(&["sparse-checkout", "init", "--cone"]);
    fixture.git(&["sparse-checkout", "set", "--skip-checks", "inside"]);
    fixture.write("outside/collision.txt", "same contents\n");
    let sandbox = rehearse(
        &fixture,
        &["merge", "--no-edit", "sparse-identical-feature"],
    );

    apply::run(&sandbox, NOW).expect("a skipped path is not checked out");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("outside/collision.txt"))
            .expect("original file"),
        "same contents\n"
    );
}

#[test]
fn a_carried_result_cannot_hide_a_collision_during_the_intermediate_reset() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "carried-feature", "main"]);
    fixture.commit_file(
        "collision.txt",
        "feature content\n",
        "add collision during reset",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("file.txt", "carried work\n");
    fixture.write("collision.txt", "user content\n");
    let mut sandbox = rehearse(&fixture, &["merge", "--no-edit", "carried-feature"]);

    // Simulate a carried conflict resolution that removes the path introduced
    // by the branch. The real apply still resets through that branch first.
    fixture.git_in(&sandbox.worktree(), &["rm", "-q", "collision.txt"]);
    let result = fixture.git_in(&sandbox.worktree(), &["stash", "create"]);
    fixture.git_in(
        &sandbox.worktree(),
        &["update-ref", "refs/rehearse/replayed", &result],
    );
    sandbox
        .record_replay(carry::Replay::Restored {
            result: Some(result),
        })
        .expect("metadata is writable");

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
}

#[cfg(unix)]
#[test]
fn collision_preflight_does_not_run_inherited_template_hooks() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "hook-feature", "main"]);
    fixture.commit_file("new.txt", "feature content\n", "add file");
    fixture.git(&["checkout", "-q", "main"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "hook-feature"]);

    let template = fixture.scratch("hostile-template");
    let hooks = template.join("hooks");
    std::fs::create_dir_all(&hooks).expect("template hooks directory");
    let marker = fixture.base().join("template-hook-ran");
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nprintf hook > '{}'\n", marker.display()),
    )
    .expect("template hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("hook metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).expect("hook executable");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_TEMPLATE_DIR", &template)
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .args(["apply", sandbox.id()])
        .output()
        .expect("git-rehearse apply runs");
    assert!(
        output.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!marker.exists(), "the inherited template hook ran");
}

#[test]
fn collision_preflight_resolves_a_separate_git_directory() {
    let fixture = Fixture::new();
    let separate = fixture.scratch("separate-git");
    let git_dir = fixture.repo().join(".git");
    std::fs::rename(&git_dir, &separate).expect("move git directory");
    std::fs::write(&git_dir, format!("gitdir: {}\n", separate.display())).expect("gitdir file");
    fixture.git(&["checkout", "-q", "-b", "separate-feature", "main"]);
    fixture.commit_file("new.txt", "feature content\n", "add file");
    fixture.git(&["checkout", "-q", "main"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "separate-feature"]);

    apply::run(&sandbox, NOW).expect("separate git directory is supported");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("new.txt")).expect("rehearsed file"),
        "feature content\n"
    );
}

#[test]
fn collision_preflight_matches_a_sha256_repository() {
    let fixture = Fixture::new();
    let repo = fixture.scratch("sha256-repo");
    fixture.git_in(
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    fixture.git_in(&repo, &["config", "user.name", "Fixture"]);
    fixture.git_in(&repo, &["config", "user.email", "fixture@example.invalid"]);
    // Like the shared fixtures, keep byte assertions independent of host Git defaults.
    fixture.git_in(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("base.txt"), "base\n").expect("base file");
    fixture.git_in(&repo, &["add", "base.txt"]);
    fixture.git_in(&repo, &["commit", "-q", "-m", "base"]);
    fixture.git_in(&repo, &["checkout", "-q", "-b", "sha256-feature"]);
    std::fs::write(repo.join("new.txt"), "feature content\n").expect("feature file");
    fixture.git_in(&repo, &["add", "new.txt"]);
    fixture.git_in(&repo, &["commit", "-q", "-m", "feature"]);
    fixture.git_in(&repo, &["checkout", "-q", "main"]);

    let plan = preflight::run(&repo)
        .expect("sha256 repository passes preflight")
        .into_plan(vec![
            "merge".to_owned(),
            "--no-edit".to_owned(),
            "sha256-feature".to_owned(),
        ]);
    let mut sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    let outcome = execute::run(&sandbox.worktree(), &plan.command, None).expect("the command runs");
    sandbox.record(&outcome).expect("the outcome is recorded");

    apply::run(&sandbox, NOW).expect("sha256 repository applies");
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).expect("rehearsed file"),
        "feature content\n"
    );
}
