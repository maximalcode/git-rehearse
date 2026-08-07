//! Reading a finished rehearsal back out of the sandbox.
//!
//! Every case here runs a real command first and then asks the analyser what
//! happened, because the whole value of the answer is that nobody inferred it
//! from the command line.

mod support;

use std::path::PathBuf;

use git_rehearse::analyze::{self, Analysis};
use git_rehearse::execute::{self, Outcome, Todo};
use git_rehearse::sandbox::{self, Sandbox};
use git_rehearse::{preflight, sandbox::Plan};
use support::Fixture;

const NOW: u64 = 1_786_248_000;

fn plan_of(fixture: &Fixture, command: &[&str]) -> Plan {
    preflight::run(fixture.repo())
        .expect("the fixture passes preflight")
        .into_plan(command.iter().map(|arg| (*arg).to_owned()).collect())
}

fn sandbox_of(fixture: &Fixture, command: &[&str]) -> (Sandbox, Plan) {
    let plan = plan_of(fixture, command);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    (sandbox, plan)
}

/// Rehearses `command` and analyses the result — the #2 → #3 → #4 → #5 path a
/// real invocation takes.
fn rehearse(fixture: &Fixture, command: &[&str]) -> (Sandbox, Analysis) {
    let (sandbox, plan) = sandbox_of(fixture, command);
    let outcome = execute::run(&sandbox.worktree(), &plan.command, None).expect("the command runs");
    let analysis = analyze::run(
        &sandbox.worktree(),
        &plan.pre_state,
        &plan.command,
        &outcome,
    )
    .expect("the sandbox can be read");
    (sandbox, analysis)
}

#[test]
fn a_merge_reports_the_branch_and_head_that_moved() {
    let fixture = Fixture::new();
    let before = fixture.git(&["rev-parse", "main"]);

    let (sandbox, analysis) = rehearse(&fixture, &["merge", "feature"]);

    let after = fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]);
    let names: Vec<&str> = analysis
        .ref_moves
        .iter()
        .map(|moved| moved.name.as_str())
        .collect();
    assert_eq!(names, vec!["HEAD", "refs/heads/main"]);
    for moved in &analysis.ref_moves {
        assert_eq!(moved.before.as_deref(), Some(before.as_str()));
        assert_eq!(moved.after.as_deref(), Some(after.as_str()));
    }
    assert!(analysis.conflicts.is_empty());
    assert!(analysis.stopped_at.is_none());
}

#[test]
fn a_merge_that_changes_content_is_not_accused_of_drift() {
    let fixture = Fixture::new();

    let (_sandbox, analysis) = rehearse(&fixture, &["merge", "feature"]);

    assert!(
        !analysis.drift_expected_empty,
        "a merge changing content is the point of a merge"
    );
    assert!(!analysis.has_unexpected_drift());
}

#[test]
fn a_command_that_changes_nothing_produces_an_empty_analysis() {
    let fixture = Fixture::new();

    // `main` is already an ancestor of itself: git does nothing at all.
    let (_sandbox, analysis) = rehearse(&fixture, &["merge", "main"]);

    assert!(analysis.ref_moves.is_empty(), "{:?}", analysis.ref_moves);
    assert!(analysis.drift.is_empty());
    assert!(analysis.conflicts.is_empty());
}

#[test]
fn a_stopped_rebase_reports_the_commit_it_stopped_on_and_the_conflict() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);

    let (sandbox, analysis) = rehearse(&fixture, &["rebase", "main"]);

    let stopped = analysis.stopped_at.expect("a stopped rebase knows where");
    assert_eq!(
        stopped.subject, "three",
        "the commit being replayed, not the one it landed on"
    );
    assert_eq!(
        stopped.sha,
        fixture.git(&["rev-parse", "feature"]),
        "and it is the real commit from the real repository"
    );
    assert_eq!(analysis.conflicts.len(), 1);
    assert_eq!(analysis.conflicts[0].path, "file.txt");
    assert_eq!(
        analysis.conflicts[0].hunks, 1,
        "one hunk, counted from the markers git wrote"
    );
    assert!(
        fixture
            .git_in(&sandbox.worktree(), &["status"])
            .contains("rebase"),
        "the sandbox is left mid-rebase for the user to look at"
    );
}

#[test]
fn a_rebase_that_only_reorders_history_shows_no_content_drift() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "feature"]);
    fixture.commit_file("a.txt", "a\n", "add a");
    fixture.commit_file("b.txt", "b\n", "add b");

    let (sandbox, plan) = sandbox_of(&fixture, &["rebase", "-i", "HEAD~2"]);
    let worktree = sandbox.worktree();
    let commits: Vec<String> = fixture
        .git_in(&worktree, &["log", "--format=%H", "--reverse", "-2"])
        .lines()
        .map(str::to_owned)
        .collect();
    // Swap two commits that touch different files: the history is rewritten,
    // the resulting tree is identical.
    let todo_file = fixture.base().join("todo");
    std::fs::write(
        &todo_file,
        format!("pick {}\npick {}\n", commits[1], commits[0]),
    )
    .expect("write the todo");
    let todo = Todo {
        file: todo_file,
        editor: PathBuf::from(env!("CARGO_BIN_EXE_git-rehearse")),
    };

    let outcome = execute::run(&worktree, &plan.command, Some(&todo)).expect("the rebase runs");
    assert_eq!(outcome, Outcome::Clean);
    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &outcome)
        .expect("the sandbox can be read");

    assert!(
        analysis
            .ref_moves
            .iter()
            .any(|moved| moved.name == "refs/heads/feature"),
        "the branch was rewritten: {:?}",
        analysis.ref_moves
    );
    assert!(
        analysis.drift.is_empty(),
        "rewriting history without changing content must be silent: {:?}",
        analysis.drift
    );
    assert!(analysis.drift_expected_empty);
    assert!(!analysis.has_unexpected_drift());
}

#[test]
fn content_that_changes_during_a_replay_is_reported_as_drift() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (sandbox, plan) = sandbox_of(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();

    let outcome = execute::run(&worktree, &plan.command, None).expect("the rebase runs");
    assert_eq!(outcome, Outcome::Stopped { conflicts: true });

    // Resolve the conflict to something neither side said — the exact way a
    // rebase silently changes what a commit meant.
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);

    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &Outcome::Clean)
        .expect("the sandbox can be read");

    assert!(
        analysis.has_unexpected_drift(),
        "this is the warning the whole tool exists for: {analysis:?}"
    );
    let drift = &analysis.drift[0];
    assert_eq!(drift.reference, "refs/heads/feature");
    assert_eq!(drift.changes.len(), 1);
    assert_eq!(drift.changes[0].path, "file.txt");
    assert_eq!(drift.changes[0].status, "M");
    // And the counts explain part of it: the new tip carries a commit the old
    // one did not, because the base moved on.
    assert_eq!(drift.commits_before, 1);
    assert_eq!(drift.commits_after, 2);
}

#[test]
fn a_deleted_branch_is_a_move_with_nowhere_to_go() {
    let fixture = Fixture::new();
    let (sandbox, plan) = sandbox_of(&fixture, &["branch", "-D", "feature"]);
    let worktree = sandbox.worktree();

    let outcome = execute::run(&worktree, &plan.command, None).expect("the command runs");
    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &outcome)
        .expect("the sandbox can be read");

    let deleted = analysis
        .ref_moves
        .iter()
        .find(|moved| moved.name == "refs/heads/feature")
        .expect("the deletion is reported");
    assert!(deleted.before.is_some());
    assert_eq!(deleted.after, None);
    assert!(
        analysis.drift.is_empty(),
        "a branch that no longer exists has no content to compare"
    );
}
