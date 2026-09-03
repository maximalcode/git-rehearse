//! Carrying uncommitted work through a rehearsal, end to end.
//!
//! These drive the real binary rather than the library, because the feature is
//! a sequence of things happening to two repositories in the right order —
//! snapshot, transfer, run, replay, transplant — and a test that called the
//! functions in that order would be asserting its own arrangement rather than
//! the tool's.
//!
//! The scenario each one is built on is the same shape: a topic branch, a base
//! that has moved on, and work in progress in the worktree. Whether the replay
//! comes back clean is then decided by *which* file the work is in.

mod support;

use git_rehearse::sandbox;
use support::Fixture;

const CLEAN: i32 = 0;
const STOPPED: i32 = 2;
const REFUSED: i32 = 4;

/// A repository where `topic` can be rebased onto `main` cleanly, and where
/// `notes.txt` is a file the rebase brings a *different* version of.
///
/// - `main` has `notes.txt` at "alpha", then moves it on to "beta".
/// - `topic` branches before that and touches `file.txt` only.
///
/// So rebasing `topic` onto `main` never conflicts, and uncommitted work in
/// `file.txt` replays cleanly while uncommitted work in `notes.txt` cannot.
fn scenario() -> Fixture {
    let fixture = Fixture::new();
    fixture.commit_file("notes.txt", "alpha\n", "add notes");
    fixture.git(&["checkout", "-q", "-b", "topic", "main"]);
    fixture.commit_file("file.txt", "topic\n", "topic work");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.commit_file("notes.txt", "beta\n", "move the notes on");
    fixture.git(&["checkout", "-q", "topic"]);
    fixture
}

/// The rehearsal id out of a report.
fn id_of(report: &str) -> String {
    report
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .unwrap_or_else(|| panic!("the report names the rehearsal:\n{report}"))
        .to_owned()
}

#[test]
fn an_untracked_file_colliding_with_a_carried_result_is_not_overwritten() {
    assert_carried_collision_is_refused("user content\n");
}

#[test]
fn identical_contents_do_not_allow_a_carried_result_to_replace_an_untracked_file() {
    assert_carried_collision_is_refused("resolution\n");
}

fn assert_carried_collision_is_refused(untracked_content: &str) {
    let fixture = scenario();
    fixture.write("notes.txt", "gamma\n");

    let (code, out, err) = fixture.rehearse(&["--keep", "rebase", "main"]);
    assert_eq!(code, STOPPED, "{err}\n{out}");
    let id = id_of(&out);
    let sandbox = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .pop()
        .expect("the rehearsal was kept");
    std::fs::write(sandbox.worktree().join("notes.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&sandbox.worktree(), &["add", "notes.txt"]);
    std::fs::write(
        sandbox.worktree().join("added-by-resolution.txt"),
        "resolution\n",
    )
    .expect("add a resolved path");
    fixture.git_in(&sandbox.worktree(), &["add", "added-by-resolution.txt"]);

    let (code, out, err) = fixture.rehearse(&["--keep", "continue", &id]);
    assert_eq!(code, CLEAN, "{err}\n{out}");

    // This untracked path was not part of the snapshot, but the carried result
    // now contains it because the user added it while resolving the replay.
    fixture.write("added-by-resolution.txt", untracked_content);
    let before = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);
    std::fs::write(&undo, "previous undo record\n").expect("existing undo record");
    let (code, _, err) = fixture.rehearse(&["apply", &id]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("untracked"), "{err}");
    assert_eq!(fixture.refs(), before, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("added-by-resolution.txt"))
            .expect("untracked file"),
        untracked_content
    );
    assert_eq!(
        std::fs::read_to_string(undo).expect("undo"),
        "previous undo record\n"
    );
    assert!(sandbox.worktree().is_dir(), "refusal retains the sandbox");
}

#[test]
fn work_in_progress_is_carried_through_a_rebase_and_comes_back() {
    let fixture = scenario();
    fixture.write("file.txt", "topic\nwork in progress\n");

    let (code, out, err) = fixture.rehearse(&["--apply", "rebase", "main"]);

    assert_eq!(code, CLEAN, "{err}\n{out}");
    assert!(
        out.contains("carried  1 uncommitted path(s): file.txt"),
        "{out}"
    );
    assert!(out.contains("come back clean"), "{out}");
    assert!(out.contains("1 uncommitted path(s) put back"), "{out}");

    // The history is the rebased history…
    assert_eq!(
        fixture
            .git(&["log", "--format=%s", "topic"])
            .lines()
            .count(),
        5
    );
    assert_eq!(fixture.git(&["show", "topic:notes.txt"]), "beta");
    // …the committed content has none of the work in progress in it…
    assert_eq!(fixture.git(&["show", "topic:file.txt"]), "topic");
    // …and the work in progress is in the worktree, uncommitted, as it was.
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        "topic\nwork in progress\n"
    );
    assert_eq!(
        fixture.git(&["status", "--porcelain", "--untracked-files=no"]),
        " M file.txt"
    );
}

#[test]
fn a_rehearsal_never_appears_in_the_users_stash_list() {
    // `git stash create` writes the commit and prints its id without touching
    // refs/stash. The stash list is the user's, and a tool that borrowed it
    // would be visible in `git stash list` forever after a crash.
    let fixture = scenario();
    fixture.write("file.txt", "topic\nwork in progress\n");
    fixture.git(&["stash", "push", "-m", "the user's own stash"]);
    fixture.write("file.txt", "topic\nsomething else\n");

    let (code, _, err) = fixture.rehearse(&["--apply", "rebase", "main"]);

    assert_eq!(code, CLEAN, "{err}");
    let list = fixture.git(&["stash", "list"]);
    assert_eq!(list.lines().count(), 1, "{list}");
    assert!(list.contains("the user's own stash"), "{list}");
}

#[test]
fn changes_the_rehearsed_history_already_contains_are_not_silently_dropped() {
    // The uncommitted change is exactly what `main` did to notes.txt. After
    // the rebase there is nothing left to put back — and the report has to say
    // so, because a worktree that comes back clean looks precisely like work
    // that was thrown away.
    let fixture = scenario();
    fixture.write("notes.txt", "beta\n");

    let (code, out, err) = fixture.rehearse(&["--apply", "rebase", "main"]);

    assert_eq!(code, CLEAN, "{err}\n{out}");
    assert!(out.contains("nothing comes back"), "{out}");
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("notes.txt")).expect("worktree"),
        "beta\n",
        "the content is there — it is in the commits now"
    );
}

#[test]
fn work_that_conflicts_with_the_rehearsed_history_stops_the_rehearsal() {
    // The decision this feature turned on: a replay that conflicts is a
    // stopped rehearsal like any other. Exit 2, sandbox kept, directions
    // printed, `continue` after resolving.
    let fixture = scenario();
    fixture.write("notes.txt", "gamma\n");
    let before = fixture.refs();

    let (code, out, err) = fixture.rehearse(&["--keep", "rebase", "main"]);

    assert_eq!(code, STOPPED, "{err}\n{out}");
    assert!(
        out.contains("the command ran, but your uncommitted changes did not go back on"),
        "{out}"
    );
    assert!(out.contains("do NOT come back clean"), "{out}");
    assert!(out.contains("notes.txt"), "{out}");
    assert!(out.contains("git rehearse continue"), "{out}");
    assert_eq!(
        fixture.refs(),
        before,
        "nothing reaches the real repository until an apply"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("notes.txt")).expect("worktree"),
        "gamma\n",
        "and the user's work is untouched, with no conflict markers in it"
    );

    // Resolve it where it is harmless, then carry on.
    let id = id_of(&out);
    let worktree = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .pop()
        .expect("the rehearsal was kept")
        .worktree();
    assert!(out.contains(&worktree.display().to_string()), "{out}");
    std::fs::write(worktree.join("notes.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "notes.txt"]);

    let (code, out, err) = fixture.rehearse(&["--apply", "continue", &id]);

    assert_eq!(code, CLEAN, "{err}\n{out}");
    assert!(out.contains("come back clean"), "{out}");
    // The resolution is the one that comes back, because apply transplants the
    // tree the sandbox produced rather than merging anything here.
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("notes.txt")).expect("worktree"),
        "resolved\n"
    );
    assert_eq!(
        fixture.git(&["status", "--porcelain", "--untracked-files=no"]),
        " M notes.txt"
    );
}

#[test]
fn continuing_a_conflicted_replay_refuses_while_anything_is_still_unmerged() {
    let fixture = scenario();
    fixture.write("notes.txt", "gamma\n");

    let (code, out, _) = fixture.rehearse(&["--keep", "rebase", "main"]);
    assert_eq!(code, STOPPED, "{out}");

    let (code, _, err) = fixture.rehearse(&["continue", &id_of(&out)]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("still unmerged"), "{err}");
    assert!(err.contains("notes.txt"), "{err}");
}

#[test]
fn editing_the_worktree_after_the_rehearsal_is_refused_at_apply() {
    // The report promised to put *those* changes back. Anything typed since
    // was never rehearsed, is in no report, and `git reset --hard` would eat
    // it — which is the same rule the ref race check follows.
    let fixture = scenario();
    fixture.write("file.txt", "topic\nwork in progress\n");

    let (code, out, err) = fixture.rehearse(&["--keep", "rebase", "main"]);
    assert_eq!(code, CLEAN, "{err}\n{out}");
    let before = fixture.refs();

    fixture.write("file.txt", "topic\nwork in progress\nand more\n");

    let (code, _, err) = fixture.rehearse(&["apply", &id_of(&out)]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("not the ones rehearsal"), "{err}");
    assert_eq!(fixture.refs(), before, "and nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        "topic\nwork in progress\nand more\n",
        "the later edit is still there"
    );
}

#[test]
fn dropping_the_carried_changes_before_applying_is_refused_too() {
    // The opposite mistake, and the same answer: applying would put back
    // changes the user has deliberately thrown away since.
    let fixture = scenario();
    fixture.write("file.txt", "topic\nwork in progress\n");

    let (code, out, err) = fixture.rehearse(&["--keep", "rebase", "main"]);
    assert_eq!(code, CLEAN, "{err}\n{out}");

    fixture.git(&["checkout", "--", "file.txt"]);

    let (code, _, err) = fixture.rehearse(&["apply", &id_of(&out)]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("no longer in your worktree"), "{err}");
}

#[test]
fn staged_and_unstaged_work_are_both_carried() {
    let fixture = scenario();
    fixture.commit_file("other.txt", "other\n", "another file");
    fixture.write("file.txt", "topic\nstaged\n");
    fixture.git(&["add", "file.txt"]);
    fixture.write("other.txt", "other\nunstaged\n");

    let (code, out, err) = fixture.rehearse(&["--apply", "rebase", "main"]);

    assert_eq!(code, CLEAN, "{err}\n{out}");
    assert!(out.contains("carried  2 uncommitted path(s)"), "{out}");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        "topic\nstaged\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("other.txt")).expect("worktree"),
        "other\nunstaged\n"
    );
    // Both come back as worktree modifications, the same place `git stash pop`
    // without `--index` leaves them. A transplanted tree does not carry the
    // staged/unstaged distinction, and inventing one would be guessing.
    assert_eq!(
        fixture.git(&["status", "--porcelain", "--untracked-files=no"]),
        " M file.txt\n M other.txt"
    );
}

#[test]
fn a_rehearsal_that_does_not_rewrite_the_checked_out_branch_leaves_the_worktree_alone() {
    // Nothing here resets anything, so there is no worktree question to ask
    // and no carried work to put back — the changes were never moved.
    let fixture = scenario();
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("file.txt", "two\nwork in progress\n");

    let (code, out, err) = fixture.rehearse(&["--apply", "rebase", "main", "topic"]);

    assert_eq!(code, CLEAN, "{err}\n{out}");
    assert!(
        out.contains("they stay where they are"),
        "the report has to say why nothing was replayed: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        "two\nwork in progress\n"
    );
    assert_eq!(
        fixture.git(&["status", "--porcelain", "--untracked-files=no"]),
        " M file.txt"
    );
}

#[test]
fn a_worktree_git_will_not_snapshot_is_refused_in_gits_own_words() {
    // Somebody already in the middle of a merge. `git stash create` will not
    // snapshot unmerged entries, and guessing at what to carry instead is
    // exactly what principle 5 forbids.
    let fixture = scenario();
    fixture.git(&["checkout", "-q", "main"]);
    fixture.commit_file("file.txt", "main's own\n", "main touches the same file");
    let merge = std::process::Command::new("git")
        .arg("-C")
        .arg(fixture.repo())
        .args(["merge", "--no-edit", "topic"])
        .output()
        .expect("git runs");
    assert!(!merge.status.success(), "the fixture must be mid-merge");

    let (code, _, err) = fixture.rehearse(&["rebase", "main"]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("cannot be snapshotted"), "{err}");
    assert!(err.contains("Finish or abort"), "{err}");
}

#[test]
fn the_json_document_says_what_was_carried_and_whether_it_comes_back() {
    let fixture = scenario();
    fixture.write("notes.txt", "gamma\n");

    let (code, out, err) = fixture.rehearse(&["--json", "rebase", "main"]);

    assert_eq!(code, STOPPED, "{err}\n{out}");
    let document: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("one document")).expect("valid JSON");
    assert_eq!(document["carried"]["status"], "conflicted");
    assert_eq!(document["carried"]["paths"][0], "notes.txt");
    assert_eq!(document["carried"]["conflicts"][0], "notes.txt");
    assert_eq!(document["can_apply"], false);
    assert_eq!(document["decision"], "kept");
}

#[test]
fn the_json_apply_says_which_paths_were_put_back() {
    let fixture = scenario();
    fixture.write("file.txt", "topic\nwork in progress\n");

    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "rebase", "main"]);
    assert_eq!(code, CLEAN, "{err}\n{out}");
    let document: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("one document")).expect("valid JSON");
    assert_eq!(document["carried"]["status"], "restored");

    let (code, out, err) = fixture.rehearse(&["--json", "apply"]);

    assert_eq!(code, CLEAN, "{err}\n{out}");
    let document: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("one document")).expect("valid JSON");
    assert_eq!(document["applied"]["carried_back"][0], "file.txt");
}
