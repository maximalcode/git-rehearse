//! The nine scenarios SCOPE.md names as v1.0's integration coverage.
//!
//! > clean rebase, conflicting rebase, `--onto`, octopus merge, cherry-pick
//! > range, rewritten checked-out branch, ref race on apply, dirty-tree
//! > refusal, evil-merge drift detection
//!
//! Eight of the nine still say what they said. The eighth changed shape with
//! #59: a dirty tree is no longer refused, it is carried, so the scenario
//! covers the whole of that instead — which is the same repository shape being
//! answered better rather than a scenario dropped.
//!
//! Each one runs the whole pipeline — preflight, sandbox, execute, analyse,
//! apply — against a repository built by running real git, because the point
//! is to find out what git actually does with these shapes rather than what
//! the modules assume it does. Every fixture is scripted; there is no
//! checked-in `.git` directory anywhere in this repository, per CLAUDE.md §6.
//!
//! The other test files check one module each. This one exists so that a
//! change which passes them all still has to survive the shapes a user
//! actually brings.

mod support;

use git_rehearse::analyze::Analysis;
use git_rehearse::carry::Replay;
use git_rehearse::execute::Outcome;
use git_rehearse::sandbox::{Plan, Sandbox};
use git_rehearse::{Error, analyze, apply, carry, execute, preflight, report, sandbox};
use support::Fixture;

const NOW: u64 = 1_786_248_000;

/// One rehearsal, all the way through.
struct Rehearsal {
    sandbox: Sandbox,
    plan: Plan,
    outcome: Outcome,
    analysis: Analysis,
}

fn rehearse(fixture: &Fixture, command: &[&str]) -> Rehearsal {
    let plan = preflight::run(fixture.repo())
        .expect("the fixture passes preflight")
        .into_plan(command.iter().map(|arg| (*arg).to_owned()).collect());
    let mut sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    let outcome = execute::run(&sandbox.worktree(), &plan.command, None).expect("the command runs");
    // The whole pipeline, which since #59 includes putting any carried
    // uncommitted work back — in the sandbox, before anybody is asked
    // anything.
    let outcome = carry::after_command(&mut sandbox, outcome).expect("the replay runs");
    let analysis = analyze::run(
        &sandbox.worktree(),
        &plan.pre_state,
        &plan.command,
        &outcome,
    )
    .expect("the sandbox can be read");
    Rehearsal {
        sandbox,
        plan,
        outcome,
        analysis,
    }
}

impl Rehearsal {
    fn moved(&self, name: &str) -> &git_rehearse::analyze::RefMove {
        self.analysis
            .ref_moves
            .iter()
            .find(|moved| moved.name == name)
            .unwrap_or_else(|| panic!("{name} should have moved: {:?}", self.analysis.ref_moves))
    }
}

/// A branch off `main` that adds one file, leaving `main` checked out.
fn branch_adding(fixture: &Fixture, branch: &str, file: &str) {
    fixture.git(&["checkout", "-q", "-b", branch, "main"]);
    fixture.commit_file(file, "content\n", &format!("add {file}"));
    fixture.git(&["checkout", "-q", "main"]);
}

// 1 ---------------------------------------------------------------- clean rebase

#[test]
fn clean_rebase() {
    let fixture = Fixture::new();
    // main moves on in a file the feature branch never touched.
    fixture.commit_file("other.txt", "other\n", "unrelated work");
    fixture.git(&["checkout", "feature"]);

    let rehearsal = rehearse(&fixture, &["rebase", "main"]);

    assert_eq!(rehearsal.outcome, Outcome::Clean);
    let moved = rehearsal.moved("refs/heads/feature");
    assert_ne!(moved.before, moved.after);
    assert!(
        !rehearsal.analysis.has_unexpected_drift(),
        "a clean replay onto a moved base is not drift: {:?}",
        rehearsal.analysis.drift
    );
    assert!(report::can_apply(&rehearsal.analysis, &rehearsal.outcome));

    // And it applies to exactly what was rehearsed.
    let rehearsed = fixture.git_in(&rehearsal.sandbox.worktree(), &["rev-parse", "feature"]);
    apply::run(&rehearsal.sandbox, NOW).expect("apply succeeds");
    assert_eq!(fixture.git(&["rev-parse", "feature"]), rehearsed);
}

// 2 ---------------------------------------------------------- conflicting rebase

#[test]
fn conflicting_rebase() {
    let fixture = Fixture::new();
    fixture.commit("conflicting work", "four\n");
    fixture.git(&["checkout", "feature"]);

    let rehearsal = rehearse(&fixture, &["rebase", "main"]);

    assert_eq!(rehearsal.outcome, Outcome::Stopped { conflicts: true });
    assert_eq!(rehearsal.analysis.conflicts.len(), 1);
    assert_eq!(rehearsal.analysis.conflicts[0].path, "file.txt");
    assert_eq!(
        rehearsal
            .analysis
            .stopped_at
            .as_ref()
            .expect("it knows where it stopped")
            .subject,
        "three"
    );
    assert!(
        !report::can_apply(&rehearsal.analysis, &rehearsal.outcome),
        "half a rebase is not a result"
    );
}

// 3 ------------------------------------------------------------- rebase --onto

#[test]
fn rebase_onto() {
    let fixture = Fixture::new();
    // feature has two commits; --onto moves only the second onto main.
    fixture.git(&["checkout", "feature"]);
    fixture.commit_file("second.txt", "second\n", "second on feature");
    fixture.git(&["checkout", "main"]);
    fixture.commit_file("other.txt", "other\n", "unrelated work");
    fixture.git(&["checkout", "feature"]);

    let rehearsal = rehearse(
        &fixture,
        &["rebase", "--onto", "main", "feature~1", "feature"],
    );

    assert_eq!(rehearsal.outcome, Outcome::Clean);
    let subjects = fixture.git_in(
        &rehearsal.sandbox.worktree(),
        &["log", "--format=%s", "main..feature"],
    );
    assert_eq!(
        subjects, "second on feature",
        "--onto keeps the last commit and drops the ones below it"
    );
    // The commit that --onto left behind is gone from the branch, and the
    // report says so rather than leaving it to be discovered.
    let drift = &rehearsal.analysis.drift;
    assert!(
        rehearsal.analysis.has_unexpected_drift(),
        "dropping a commit is exactly what the warning is for: {drift:?}"
    );
    assert!(
        drift[0].replay.dropped.contains(&"three".to_owned()),
        "{drift:?}"
    );
}

// 4 ------------------------------------------------------------- octopus merge

#[test]
fn octopus_merge() {
    let fixture = Fixture::new();
    branch_adding(&fixture, "one-more", "one-more.txt");
    branch_adding(&fixture, "another", "another.txt");
    // main needs a commit of its own: git's octopus strategy fast-forwards to
    // the first branch when HEAD is its ancestor, and the result is an
    // ordinary two-parent merge rather than the shape this scenario is about.
    fixture.commit_file("main-only.txt", "main\n", "main moves too");

    let rehearsal = rehearse(&fixture, &["merge", "--no-edit", "one-more", "another"]);

    assert_eq!(rehearsal.outcome, Outcome::Clean);
    let parents = fixture.git_in(
        &rehearsal.sandbox.worktree(),
        &["rev-list", "--parents", "-1", "main"],
    );
    assert_eq!(
        parents.split_whitespace().count(),
        4,
        "an octopus merge commit has three parents: {parents}"
    );
    // The graph renderer is git's, so a three-parent commit has to come out
    // right without any help from us.
    let graphs = report::graphs(
        &rehearsal.sandbox.worktree(),
        &rehearsal.analysis,
        report::Detail::Full,
    )
    .expect("graphs render");
    let text = report::render(
        rehearsal.sandbox.meta(),
        &rehearsal.analysis,
        &rehearsal.outcome,
        &graphs,
    );
    assert!(text.contains("Merge branches"), "{text}");
    assert!(
        !text.contains("warning:"),
        "a merge changes content: {text}"
    );
}

// 5 --------------------------------------------------------- cherry-pick range

#[test]
fn cherry_pick_range() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "feature"]);
    fixture.commit_file("second.txt", "second\n", "second on feature");
    fixture.git(&["checkout", "main"]);

    let rehearsal = rehearse(&fixture, &["cherry-pick", "main..feature"]);

    assert_eq!(rehearsal.outcome, Outcome::Clean);
    let subjects: Vec<String> = fixture
        .git_in(
            &rehearsal.sandbox.worktree(),
            &["log", "--format=%s", "-2", "main"],
        )
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        subjects,
        vec!["second on feature".to_owned(), "three".to_owned()],
        "both commits of the range were picked"
    );
    let moved = rehearsal.moved("refs/heads/main");
    assert_ne!(moved.before, moved.after);
    assert!(
        rehearsal
            .analysis
            .ref_moves
            .iter()
            .all(|moved| moved.name != "refs/heads/feature"),
        "cherry-pick copies; it must not move the branch it copied from"
    );
}

// 6 ------------------------------------------------ rewritten checked-out branch

#[test]
fn rewritten_checked_out_branch() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "unrelated work");
    fixture.git(&["checkout", "feature"]);
    let rehearsal = rehearse(&fixture, &["rebase", "main"]);

    let applied = apply::run(&rehearsal.sandbox, NOW).expect("apply succeeds");

    assert_eq!(applied.reset.as_deref(), Some("feature"));
    assert_eq!(
        fixture.git(&["status", "--porcelain"]),
        "",
        "the worktree and index follow the branch that was rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("other.txt")).expect("real worktree"),
        "other\n",
        "including the files the new base brought with it"
    );
}

// 7 ------------------------------------------------------------ ref race on apply

#[test]
fn ref_race_on_apply() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "unrelated work");
    let rehearsal = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    // Somebody commits between the report and the decision.
    fixture.commit_file("later.txt", "later\n", "meanwhile");
    let before = fixture.refs();

    let error = apply::run(&rehearsal.sandbox, NOW).expect_err("refused");

    assert!(matches!(error, Error::Refused(_)), "{error:?}");
    assert!(
        error.to_string().contains("refs/heads/main is now"),
        "{error}"
    );
    assert_eq!(fixture.refs(), before, "and nothing moved");
}

// 8 --------------------------------------------------------- dirty tree carried

#[test]
fn dirty_tree_carried_through_and_put_back() {
    let fixture = Fixture::new();
    // main moves on; the feature branch has half-finished work in it.
    fixture.commit_file("other.txt", "other\n", "unrelated work");
    fixture.git(&["checkout", "feature"]);
    fixture.write("file.txt", "three\nhalf-finished work\n");
    fixture.write("scratch.txt", "notes\n");

    let rehearsal = rehearse(&fixture, &["rebase", "main"]);

    assert_eq!(rehearsal.outcome, Outcome::Clean);
    let carry = rehearsal
        .sandbox
        .meta()
        .carry
        .as_ref()
        .expect("the uncommitted work was carried");
    assert_eq!(
        carry.paths,
        ["file.txt"],
        "tracked changes only — scratch.txt is untracked and stays out"
    );
    assert!(
        matches!(carry.replay, Some(Replay::Restored { result: Some(_) })),
        "{:?}",
        carry.replay
    );
    // The command itself ran against a clean tree: the rehearsed commit is the
    // committed history, with none of the work in progress baked into it.
    assert_eq!(
        fixture.git_in(&rehearsal.sandbox.worktree(), &["show", "feature:file.txt"]),
        "three"
    );

    apply::run(&rehearsal.sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        "three\nhalf-finished work\n",
        "the work in progress is back, on top of the rebased history"
    );
    assert_eq!(
        fixture.git(&["status", "--porcelain", "--untracked-files=no"]),
        " M file.txt",
        "and it is uncommitted, exactly as it was"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("scratch.txt")).expect("untracked file"),
        "notes\n",
        "untracked files are neither carried nor disturbed"
    );
}

// 9 -------------------------------------------------- evil-merge drift detection

#[test]
fn evil_merge_drift_detection() {
    let fixture = Fixture::new();
    fixture.commit("conflicting work", "four\n");
    fixture.git(&["checkout", "feature"]);
    let rehearsal = rehearse(&fixture, &["rebase", "main"]);
    let worktree = rehearsal.sandbox.worktree();

    // The evil resolution: content neither side ever had, introduced while
    // resolving. This is the silent semantic change the tool exists to catch.
    std::fs::write(worktree.join("file.txt"), "something nobody wrote\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);

    let analysis = analyze::run(
        &worktree,
        &rehearsal.plan.pre_state,
        &rehearsal.plan.command,
        &Outcome::Clean,
    )
    .expect("the sandbox can be read");

    assert!(analysis.has_unexpected_drift(), "{analysis:?}");
    assert_eq!(analysis.drift[0].replay.changed, vec!["three".to_owned()]);
    let text = report::render(rehearsal.sandbox.meta(), &analysis, &Outcome::Clean, &[]);
    assert!(text.contains("warning: content drift"), "{text}");
    assert!(text.contains("changed  three"), "{text}");
}

// 10 -------------------------------------------------------- signed history (#16)

#[test]
fn a_rebase_applied_into_a_signing_repository_stays_signed() {
    let fixture = Fixture::new();
    // A repository that signs *locally* — the case `git clone` does not carry,
    // so the sandbox knew nothing about it and the commits it produced were
    // unsigned.
    fixture.sign_with_ssh();
    fixture.commit_file("other.txt", "other\n", "unrelated work");
    fixture.git(&["checkout", "feature"]);

    let rehearsal = rehearse(&fixture, &["rebase", "main"]);
    assert_eq!(rehearsal.outcome, Outcome::Clean);
    assert!(
        fixture.is_signed(&rehearsal.sandbox.worktree(), "feature"),
        "the sandbox has to sign: these are the very commits apply transplants"
    );

    apply::run(&rehearsal.sandbox, NOW).expect("apply succeeds");

    // The whole point of the issue. Apply is a ref transplant, so whatever the
    // sandbox committed is now the real repository's history — and a tool that
    // silently downgrades signed history to unsigned is doing the exact thing
    // it exists to warn about.
    assert!(
        fixture.is_signed(fixture.repo(), "feature"),
        "a rehearsed rebase must not quietly strip the signatures off a branch"
    );
}
