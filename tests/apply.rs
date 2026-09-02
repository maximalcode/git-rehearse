//! Applying a rehearsal to a real repository.
//!
//! Design principle 2 is inviolable — **apply is a ref transplant, never a
//! re-run** — so most of these tests exist to prove that in ways a re-run
//! could not fake, and the rest exist to prove that a repository which moved
//! on is left alone.

mod support;

use git_rehearse::sandbox::{Plan, Sandbox};
use git_rehearse::{Error, apply, execute, preflight, sandbox};
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
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    execute::run(&sandbox.worktree(), &plan.command, None).expect("the command runs");
    sandbox
}

fn refusal(error: Error) -> String {
    match error {
        Error::Refused(message) => message,
        other => panic!("expected a refusal, got: {other:?}"),
    }
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
    let sandbox = rehearse(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();
    // Finish the rebase the fixture's conflict stopped.
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);

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
    let sandbox = rehearse(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);
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
