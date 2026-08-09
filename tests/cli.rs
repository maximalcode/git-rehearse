//! The binary, run as a user or a script would run it.
//!
//! These tests exist mainly for the exit codes. SCOPE.md fixes them from v0.1
//! on and v2's agent mode reads them, so they are API — and the only honest
//! way to test an exit code is to let a process exit with it.

mod support;

use git_rehearse::sandbox;
use support::Fixture;

/// 0 clean, 1 internal, 2 stopped, 3 failed, 4 refused.
const CLEAN: i32 = 0;
const STOPPED: i32 = 2;
const FAILED: i32 = 3;
const REFUSED: i32 = 4;

#[test]
fn a_clean_rehearsal_prints_a_report_and_exits_zero() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");

    let (code, out, _) = fixture.rehearse(&["merge", "--no-edit", "feature"]);

    assert_eq!(code, CLEAN);
    assert!(
        out.contains("rehearsed  git merge --no-edit feature"),
        "{out}"
    );
    assert!(out.contains("refs/heads/main"), "{out}");
    assert!(out.contains("graph  refs/heads/main"), "{out}");
}

#[test]
fn a_rehearsal_that_stops_on_a_conflict_exits_two() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);

    let (code, out, _) = fixture.rehearse(&["rebase", "main"]);

    assert_eq!(code, STOPPED, "{out}");
    assert!(out.contains("stopped part-way on a conflict"), "{out}");
    assert!(out.contains("file.txt  1 hunk"), "{out}");
}

#[test]
fn a_command_git_refuses_exits_three() {
    let fixture = Fixture::new();

    let (code, out, _) = fixture.rehearse(&["merge", "no-such-branch"]);

    assert_eq!(code, FAILED, "{out}");
    assert!(out.contains("git refused the command"), "{out}");
}

#[test]
fn our_own_refusal_exits_four_and_explains_itself_on_stderr() {
    let fixture = Fixture::new();
    fixture.write("file.txt", "uncommitted\n");

    let (code, _, err) = fixture.rehearse(&["merge", "feature"]);

    assert_eq!(code, REFUSED);
    assert!(err.contains("uncommitted changes"), "{err}");
    assert!(err.contains("commit or stash"), "{err}");
}

#[test]
fn an_unknown_command_is_refused_with_a_way_forward() {
    let fixture = Fixture::new();

    let (code, _, err) = fixture.rehearse(&["bisect", "start"]);

    assert_eq!(code, REFUSED);
    assert!(err.contains("git rehearse -- bisect"), "{err}");
}

#[test]
fn without_a_terminal_a_rehearsal_is_discarded_unless_asked_otherwise() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.refs();

    let (code, out, err) = fixture.rehearse(&["merge", "--no-edit", "feature"]);

    assert_eq!(fixture.refs(), before, "nothing was applied");
    let (_, listed, _) = fixture.rehearse(&["list"]);
    assert!(
        listed.contains("no rehearsals"),
        "and nothing was left in the cache: {listed}"
    );
    // Exit 0 with an unchanged repository is indistinguishable from having
    // applied, unless the run says a question was skipped.
    assert_eq!(code, CLEAN, "{err}");
    assert!(
        out.contains("stdin is not a terminal") && out.contains("discarded"),
        "the skipped question has to be visible: {out}"
    );
    assert!(
        out.contains("--apply") && out.contains("--keep"),
        "and it has to name the two ways to script it: {out}"
    );
}

#[test]
fn asking_for_a_decision_up_front_is_not_a_skipped_question() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");

    // --apply and --keep are the scripted paths. Nothing was decided on the
    // user's behalf, so the notice would be noise.
    let (_, applied, _) = fixture.rehearse(&["--apply", "merge", "--no-edit", "feature"]);
    let (_, kept, _) = fixture.rehearse(&["--keep", "merge", "--no-edit", "feature"]);

    assert!(!applied.contains("nobody to ask"), "{applied}");
    assert!(!kept.contains("nobody to ask"), "{kept}");
}

#[test]
fn the_apply_flag_moves_the_real_refs_to_the_rehearsed_commits() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.git(&["rev-parse", "main"]);

    let (code, out, err) = fixture.rehearse(&["--apply", "merge", "--no-edit", "feature"]);

    assert_eq!(code, CLEAN, "{err}");
    assert!(out.contains("applied:"), "{out}");
    assert!(out.contains("rehearse-undo"), "{out}");
    assert_ne!(fixture.git(&["rev-parse", "main"]), before);
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%s", "main"]),
        "Merge branch 'feature'"
    );
}

#[test]
fn applying_something_that_stopped_is_refused_rather_than_half_done() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let before = fixture.refs();

    let (code, _, err) = fixture.rehearse(&["--apply", "rebase", "main"]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("nothing that can be applied"), "{err}");
    assert_eq!(fixture.refs(), before);
}

#[test]
fn a_kept_rehearsal_can_be_listed_shown_and_applied_later() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.git(&["rev-parse", "main"]);

    let (code, out, err) = fixture.rehearse(&["--keep", "merge", "--no-edit", "feature"]);
    assert_eq!(code, CLEAN, "{err}");
    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();

    let (_, listed, _) = fixture.rehearse(&["list"]);
    assert!(listed.contains(&id), "{listed}");
    assert!(listed.contains("Kept"), "{listed}");

    // Shown from a later process, out of meta.json, without re-running git.
    let (code, shown, err) = fixture.rehearse(&["show", &id]);
    assert_eq!(code, CLEAN, "{err}");
    assert!(
        shown.contains("rehearsed  git merge --no-edit feature"),
        "{shown}"
    );

    // A prefix of the id is enough, because nobody wants to retype it.
    let (code, applied, err) = fixture.rehearse(&["apply", &id[..6]]);
    assert_eq!(code, CLEAN, "{err}");
    assert!(applied.contains("applied:"), "{applied}");
    assert_ne!(fixture.git(&["rev-parse", "main"]), before);
}

#[test]
fn discard_all_empties_the_cache_for_this_repository() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    fixture.rehearse(&["--keep", "merge", "--no-edit", "feature"]);
    fixture.rehearse(&["--keep", "merge", "--no-edit", "feature"]);

    let (code, out, err) = fixture.rehearse(&["discard", "--all"]);

    assert_eq!(code, CLEAN, "{err}");
    assert!(out.contains("discarded 2"), "{out}");
    let (_, listed, _) = fixture.rehearse(&["list"]);
    assert!(listed.contains("no rehearsals"), "{listed}");
}

#[test]
fn help_and_version_work_and_say_what_the_exit_codes_mean() {
    let fixture = Fixture::new();

    let (code, help, _) = fixture.rehearse(&["--help"]);
    assert_eq!(code, CLEAN);
    assert!(
        help.contains("git rehearse [options] rebase|merge|cherry-pick"),
        "{help}"
    );
    assert!(help.contains("4 refused"), "{help}");
    // The two things about the exit codes that surprise people: 0 does not
    // mean "applied", and a pipe answers the question for you.
    assert!(
        help.contains("no terminal on stdin") && help.contains("discarded"),
        "{help}"
    );
    assert!(
        help.contains("describes the rehearsal, not what became of it"),
        "{help}"
    );

    let (code, version, _) = fixture.rehearse(&["--version"]);
    assert_eq!(code, CLEAN);
    assert!(version.starts_with("git-rehearse "), "{version}");
}

#[test]
fn a_rehearsal_never_touches_the_real_repository_before_the_decision() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let before = fixture.refs();
    let worktree = std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree");

    fixture.rehearse(&["rebase", "main"]);

    assert_eq!(fixture.refs(), before);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        worktree,
        "no conflict markers in the real worktree"
    );
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
}

#[test]
fn a_stopped_rehearsal_is_resolved_in_the_sandbox_and_continued() {
    let fixture = Fixture::new();
    // main and feature both changed file.txt, so the rebase stops.
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let before = fixture.refs();

    let (code, out, err) = fixture.rehearse(&["--keep", "rebase", "main"]);
    assert_eq!(code, STOPPED, "{err}");

    // The report has to say where the conflict is and what to type next.
    // Naming the unmerged files without saying where they are is half a tool.
    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();
    let sandbox = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .pop()
        .expect("the kept rehearsal is there");
    let worktree = sandbox.worktree();
    assert!(
        out.contains(&worktree.display().to_string()),
        "the report must say where the sandbox is: {out}"
    );
    assert!(out.contains("git rehearse continue"), "{out}");

    // Resolve it exactly as a person would: edit the file, stage it.
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);

    let (code, cont, err) = fixture.rehearse(&["--keep", "continue", &id]);

    assert_eq!(code, CLEAN, "{err}\n{cont}");
    assert_eq!(
        fixture.refs(),
        before,
        "continuing must not touch the real repository — only apply does that"
    );

    // And what gets applied is what was resolved, because apply transplants
    // the sandbox's commits rather than replaying the command.
    let (code, applied, err) = fixture.rehearse(&["apply", &id]);
    assert_eq!(code, CLEAN, "{err}\n{applied}");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("worktree"),
        "resolved\n"
    );
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%s", "feature"]),
        "three"
    );
}

#[test]
fn continuing_with_a_conflict_still_unresolved_is_refused_and_names_the_files() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (_, out, _) = fixture.rehearse(&["--keep", "rebase", "main"]);
    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();

    // Nothing resolved, nothing staged.
    let (code, _, err) = fixture.rehearse(&["continue", &id]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("still unmerged"), "{err}");
    assert!(
        err.contains("file.txt"),
        "naming the file beats git's generic advice: {err}"
    );
}

#[test]
fn continuing_something_that_is_not_stopped_is_refused() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let (_, out, _) = fixture.rehearse(&["--keep", "merge", "--no-edit", "feature"]);
    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();

    let (code, _, err) = fixture.rehearse(&["continue", &id]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("nothing in progress"), "{err}");
}
