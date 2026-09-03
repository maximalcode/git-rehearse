//! Taking an apply back.
//!
//! Undo is the inverse of the transplant, so these tests are the mirror image
//! of `apply.rs`'s: that the refs land on exactly the commits they came from,
//! that a repository which moved on is left alone, and that the record behaves
//! like the single-use thing it is.

mod support;

use std::collections::BTreeMap;

use git_rehearse::sandbox::{Plan, Sandbox};
use git_rehearse::{Error, apply, execute, preflight, sandbox, undo};
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

/// The branches only.
///
/// Not every ref: an apply leaves `refs/rehearse/<id>/*` behind as an anchor,
/// on purpose, and an undo has no business removing it — the rehearsed commits
/// are what a re-apply would need.
fn branches(fixture: &Fixture) -> BTreeMap<String, String> {
    fixture
        .refs()
        .into_iter()
        .filter(|(name, _)| name.starts_with("refs/heads/"))
        .collect()
}

fn refusal(error: Error) -> String {
    match error {
        Error::Refused(message) => message,
        other => panic!("expected a refusal, got: {other:?}"),
    }
}

#[test]
fn an_apply_is_put_back_on_exactly_the_commits_it_came_from() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = branches(&fixture);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");
    assert_ne!(branches(&fixture), before);

    let undone = undo::run(fixture.repo(), None).expect("undo succeeds");

    assert_eq!(
        branches(&fixture),
        before,
        "every branch is back on the commit it was on"
    );
    assert_eq!(undone.rehearsal, sandbox.id());
    assert_eq!(undone.applied_at_unix, NOW);
    // The merge commit is not destroyed, only unreferenced — and the anchor
    // apply left behind still names it, so re-applying is not a re-rehearsal.
    fixture.git(&["cat-file", "-e", &format!("{}main", applied.anchor)]);
}

#[test]
fn the_reflog_says_the_branch_was_put_back_rather_than_jumping() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");

    undo::run(fixture.repo(), None).expect("undo succeeds");

    let reflog = fixture.git(&["reflog", "show", "--format=%gs", "main"]);
    assert!(
        reflog.contains(&format!("git-rehearse undo {}", sandbox.id())),
        "a branch that moves backwards on its own is a mystery: {reflog}"
    );
}

#[test]
fn the_worktree_follows_the_branch_it_is_standing_on() {
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
    apply::run(&sandbox, NOW).expect("apply succeeds");

    let undone = undo::run(fixture.repo(), None).expect("undo succeeds");

    assert_eq!(undone.reset.as_deref(), Some("feature"));
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "three\n",
        "the rebase is gone from the worktree, not only from the ref"
    );
    assert_eq!(
        fixture.git(&["status", "--porcelain"]),
        "",
        "and the index agrees with it"
    );
}

#[test]
fn uncommitted_work_is_never_destroyed_by_an_undo() {
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
    apply::run(&sandbox, NOW).expect("apply succeeds");
    // Second thoughts arrive after the editing has started.
    fixture.write("file.txt", "work in progress\n");
    let before = branches(&fixture);

    let message = refusal(undo::run(fixture.repo(), None).expect_err("refused"));

    assert!(message.contains("reset --hard"), "{message}");
    assert!(message.contains("Commit or stash"), "{message}");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "work in progress\n",
        "the edit is still there"
    );
    assert_eq!(branches(&fixture), before, "and nothing moved");
    // And the way back is still written down, which is the whole point of
    // refusing rather than half-doing it.
    undo::run(fixture.repo(), None).expect_err("still refused, and still recorded");
}

#[test]
fn a_commit_made_since_the_apply_stops_everything() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");
    // Work carries on, on top of what was applied.
    fixture.commit_file("later.txt", "later\n", "five");
    let after = branches(&fixture);

    let message = refusal(undo::run(fixture.repo(), None).expect_err("refused"));

    assert!(message.contains("refs/heads/main is now"), "{message}");
    assert!(message.contains("the apply left it at"), "{message}");
    assert_eq!(
        branches(&fixture),
        after,
        "a refused undo leaves the repository exactly as it found it"
    );
}

#[test]
fn a_branch_the_apply_created_is_removed_again() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "spike", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");
    assert!(branches(&fixture).contains_key("refs/heads/spike"));

    undo::run(fixture.repo(), None).expect("undo succeeds");

    assert!(
        !branches(&fixture).contains_key("refs/heads/spike"),
        "{:?}",
        branches(&fixture)
    );
}

#[test]
fn a_branch_the_apply_deleted_comes_back_where_it_was() {
    let fixture = Fixture::new();
    let was = fixture.git(&["rev-parse", "feature"]);
    let sandbox = rehearse(&fixture, &["branch", "-D", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");
    assert!(!branches(&fixture).contains_key("refs/heads/feature"));

    undo::run(fixture.repo(), None).expect("undo succeeds");

    assert_eq!(fixture.git(&["rev-parse", "feature"]), was);
}

#[test]
fn a_branch_recreated_since_the_apply_deleted_it_is_not_overwritten() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "-D", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");
    // The same name, used for something else in the meantime.
    fixture.git(&["branch", "feature", "main"]);
    let theirs = fixture.git(&["rev-parse", "feature"]);

    let message = refusal(undo::run(fixture.repo(), None).expect_err("refused"));

    assert!(
        message.contains("refs/heads/feature is back at"),
        "{message}"
    );
    assert_eq!(
        fixture.git(&["rev-parse", "feature"]),
        theirs,
        "their branch is untouched"
    );
}

#[test]
fn undoing_onto_a_branch_that_would_disappear_is_refused() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "spike", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");
    // The user goes to work on the branch the apply created for them.
    fixture.git(&["checkout", "spike"]);

    let message = refusal(undo::run(fixture.repo(), None).expect_err("refused"));

    assert!(message.contains("would delete spike"), "{message}");
    assert!(message.contains("Check out another branch"), "{message}");
    assert_eq!(fixture.git(&["rev-parse", "--abbrev-ref", "HEAD"]), "spike");
}

#[test]
fn undoing_twice_says_there_is_nothing_to_undo() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");

    undo::run(fixture.repo(), None).expect("the first undo succeeds");
    let restored = branches(&fixture);
    let message = refusal(undo::run(fixture.repo(), None).expect_err("the second is refused"));

    // The record is used up, so the second one is not an unsafe repeat of the
    // first — it is a clean nothing.
    assert!(message.contains("nothing to undo"), "{message}");
    assert!(message.contains("apply"), "{message}");
    assert_eq!(branches(&fixture), restored);
}

#[test]
fn a_second_apply_overwrites_the_record_and_undo_says_which_one_it_took_back() {
    // The documented property: one level deep. A fixed filename cannot hold a
    // history, and pretending otherwise would make the second undo restore a
    // state nobody asked for.
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let first = rehearse(&fixture, &["branch", "spike", "feature"]);
    apply::run(&first, NOW).expect("the first apply succeeds");
    let between = branches(&fixture);

    let second = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    apply::run(&second, NOW + 60).expect("the second apply succeeds");

    let undone = undo::run(fixture.repo(), None).expect("undo succeeds");

    assert_eq!(undone.rehearsal, second.id(), "the most recent apply");
    assert_eq!(undone.applied_at_unix, NOW + 60);
    assert_eq!(
        branches(&fixture),
        between,
        "back to where the second apply started, not to where the first did"
    );
    // And there is no second step back: the first record is gone.
    let message = refusal(undo::run(fixture.repo(), None).expect_err("refused"));
    assert!(message.contains("nothing to undo"), "{message}");
}

#[test]
fn undoing_an_apply_other_than_the_one_meant_is_refused_by_name() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");
    let after = branches(&fixture);

    let message = refusal(undo::run(fixture.repo(), Some("1000000000-99")).expect_err("refused"));

    assert!(message.contains(sandbox.id()), "{message}");
    assert!(
        message.contains("applying again overwrites it"),
        "{message}"
    );
    assert_eq!(branches(&fixture), after, "and nothing moved");

    // The id it does name works, as a prefix, like every other command's.
    let prefix = &sandbox.id()[..6];
    undo::run(fixture.repo(), Some(prefix)).expect("undo succeeds");
}

#[test]
fn an_apply_can_be_undone_after_its_rehearsal_is_gone() {
    // The record is in the repository, not in the cache. This is why apply
    // writes it down instead of leaning on the sandbox: the sandbox is
    // disposable by design, and by the time somebody wants an undo it usually
    // has been disposed of.
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = branches(&fixture);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    apply::run(&sandbox, NOW).expect("apply succeeds");
    sandbox.discard().expect("the sandbox is thrown away");

    undo::run(fixture.repo(), None).expect("undo succeeds");

    assert_eq!(branches(&fixture), before);
}

#[test]
fn a_record_from_a_version_this_build_does_not_know_is_refused() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");
    let after = branches(&fixture);

    let record = std::fs::read_to_string(&applied.undo).expect("the record exists");
    std::fs::write(&applied.undo, record.replace("version 1", "version 99")).expect("rewrite");

    let message = refusal(undo::run(fixture.repo(), None).expect_err("refused"));

    assert!(message.contains("record version 99"), "{message}");
    assert!(message.contains("Nothing has been changed"), "{message}");
    assert_eq!(branches(&fixture), after);
}

#[test]
fn the_record_is_usable_by_hand_exactly_as_it_says() {
    // The format's one promise to a human: a line of it, pasted after
    // `git update-ref`, puts that ref back — and git refuses if the ref has
    // moved on since.
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.git(&["rev-parse", "main"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    let record = std::fs::read_to_string(&applied.undo).expect("the record exists");
    let line = record
        .lines()
        .find(|line| line.starts_with("refs/heads/main "))
        .expect("main is in the record")
        .to_owned();
    let arguments: Vec<&str> = line.split_whitespace().collect();

    assert_ne!(fixture.git(&["rev-parse", "main"]), before);
    let mut command = vec!["update-ref"];
    command.extend(arguments.iter().copied());
    fixture.git(&command);

    assert_eq!(fixture.git(&["rev-parse", "main"]), before);
    // HEAD is deliberately absent: it follows its branch, and a line telling
    // somebody to `git update-ref HEAD` would detach it.
    assert!(
        !record.lines().any(|line| line.starts_with("HEAD ")),
        "{record}"
    );
}
