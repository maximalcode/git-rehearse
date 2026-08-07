//! Running real commands in real sandboxes.
//!
//! The point of these tests is that git does the work and we only classify the
//! result: a clean merge, a merge that stops on a conflict, a command git
//! refuses, and an interactive rebase driven by an injected todo — each run by
//! the actual `git` binary against an actual repository.

mod support;

use std::path::PathBuf;

use git_rehearse::execute::{self, Outcome, Todo};
use git_rehearse::sandbox::{self, Sandbox};
use git_rehearse::{Error, preflight};
use support::Fixture;

const NOW: u64 = 1_786_248_000;

/// A sandbox of the fixture, checked out where the fixture is.
fn sandbox_of(fixture: &Fixture, command: &[&str]) -> Sandbox {
    let plan = preflight::run(fixture.repo())
        .expect("the fixture passes preflight")
        .into_plan(command.iter().map(|arg| (*arg).to_owned()).collect());
    sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created")
}

fn command(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn a_clean_merge_is_clean_and_moves_the_sandbox_branch() {
    let fixture = Fixture::new();
    let sandbox = sandbox_of(&fixture, &["merge", "feature"]);
    let before = fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]);

    let outcome =
        execute::run(&sandbox.worktree(), &command(&["merge", "feature"]), None).expect("git runs");

    assert_eq!(outcome, Outcome::Clean);
    assert_ne!(
        fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]),
        before,
        "the rehearsal actually happened"
    );
    assert_eq!(
        fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]),
        fixture.git_in(&sandbox.worktree(), &["rev-parse", "feature"]),
        "a fast-forward merge lands on the feature tip"
    );
}

#[test]
fn a_conflicting_merge_stops_with_conflicts_and_leaves_it_to_look_at() {
    let fixture = Fixture::new();
    // main and feature now both rewrote the same line from a common ancestor.
    fixture.commit("four", "four\n");
    let sandbox = sandbox_of(&fixture, &["merge", "feature"]);

    let outcome =
        execute::run(&sandbox.worktree(), &command(&["merge", "feature"]), None).expect("git runs");

    assert_eq!(outcome, Outcome::Stopped { conflicts: true });
    assert!(
        fixture
            .git_in(&sandbox.worktree(), &["ls-files", "--unmerged"])
            .contains("file.txt"),
        "the conflict is still there for the report to describe"
    );
}

#[test]
fn a_command_git_refuses_is_failed_not_stopped() {
    let fixture = Fixture::new();
    let sandbox = sandbox_of(&fixture, &["merge", "no-such-ref"]);

    let outcome = execute::run(
        &sandbox.worktree(),
        &command(&["merge", "no-such-ref"]),
        None,
    )
    .expect("git runs");

    // Nothing in progress, nothing to inspect: this is exit 3, not exit 2.
    match outcome {
        Outcome::Failed { code } => assert!(code.is_some_and(|code| code != 0), "{code:?}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn nothing_that_runs_in_the_sandbox_reaches_the_real_repository() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    let before = fixture.refs();
    let sandbox = sandbox_of(&fixture, &["merge", "feature"]);

    execute::run(&sandbox.worktree(), &command(&["merge", "feature"]), None).expect("git runs");

    assert_eq!(
        fixture.refs(),
        before,
        "the rehearsal stayed in the sandbox"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "four\n",
        "no conflict markers in the real worktree"
    );
}

#[test]
fn an_injected_todo_drives_an_interactive_rebase_without_an_editor() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "feature"]);
    fixture.commit("five", "five\n");
    fixture.git(&["checkout", "main"]);
    fixture.git(&["checkout", "feature"]);

    let sandbox = sandbox_of(&fixture, &["rebase", "-i", "main"]);
    let worktree = sandbox.worktree();
    // Two commits sit on top of main; the todo keeps the first and drops the
    // second, which is the whole point of rehearsing a todo.
    let commits: Vec<String> = fixture
        .git_in(
            &worktree,
            &["log", "--format=%H", "--reverse", "main..feature"],
        )
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(commits.len(), 2, "fixture has two commits to rebase");

    let todo_file = fixture.base().join("todo");
    std::fs::write(&todo_file, format!("pick {}\n", commits[0])).expect("write the todo");
    let todo = Todo {
        file: todo_file,
        // Under `cargo test` the running executable is the test harness, so
        // the binary under test is named explicitly. Todo::new does this for
        // real invocations.
        editor: PathBuf::from(env!("CARGO_BIN_EXE_git-rehearse")),
    };

    let outcome = execute::run(&worktree, &command(&["rebase", "-i", "main"]), Some(&todo))
        .expect("git runs");

    assert_eq!(outcome, Outcome::Clean);
    let after: Vec<String> = fixture
        .git_in(&worktree, &["log", "--format=%s", "main..feature"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(after, vec!["three".to_owned()], "the todo was obeyed");
}

#[test]
fn a_todo_that_git_would_ignore_is_refused_before_anything_runs() {
    let fixture = Fixture::new();
    let sandbox = sandbox_of(&fixture, &["rebase", "main"]);
    let todo_file = fixture.base().join("todo");
    std::fs::write(&todo_file, "pick deadbeef\n").expect("write the todo");
    let todo = Todo {
        file: todo_file,
        editor: PathBuf::from(env!("CARGO_BIN_EXE_git-rehearse")),
    };
    let before = fixture.git_in(&sandbox.worktree(), &["rev-parse", "HEAD"]);

    // No -i: git would generate its own todo and silently ignore ours, so the
    // rehearsal would report on a rebase the user did not ask for.
    let error = execute::run(
        &sandbox.worktree(),
        &command(&["rebase", "main"]),
        Some(&todo),
    )
    .expect_err("refused");

    assert!(matches!(error, Error::Refused(_)), "{error:?}");
    assert!(error.to_string().contains("Add -i"), "{error}");
    assert_eq!(
        fixture.git_in(&sandbox.worktree(), &["rev-parse", "HEAD"]),
        before,
        "the refusal happened before git ran"
    );
}

#[test]
fn a_rebase_that_stops_on_a_conflict_reports_conflicts() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    let sandbox = sandbox_of(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();
    fixture.git_in(&worktree, &["checkout", "feature"]);

    let outcome = execute::run(&worktree, &command(&["rebase", "main"]), None).expect("git runs");

    assert_eq!(outcome, Outcome::Stopped { conflicts: true });
    // And git left the rebase in progress rather than unwinding it, so #5 has
    // something to analyse and the user something to resolve.
    assert!(
        fixture.git_in(&worktree, &["status"]).contains("rebase"),
        "the sandbox is mid-rebase"
    );
}
