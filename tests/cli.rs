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
    fixture.commit_file(
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "track binaries with lfs",
    );

    let (code, _, err) = fixture.rehearse(&["merge", "feature"]);

    assert_eq!(code, REFUSED);
    assert!(err.contains("Git LFS"), "{err}");
    assert!(err.contains("not supported in v1"), "{err}");
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
fn applying_a_kept_stopped_cherry_pick_is_refused_and_keeps_its_sandbox() {
    let fixture = Fixture::new();
    // The first source commit applies to main; the second changes the same
    // line as a commit made on main, so cherry-pick stops with main partially
    // advanced in the sandbox.
    fixture.git(&["checkout", "-q", "-b", "pick-source"]);
    fixture.commit_file("first.txt", "first\n", "first pick");
    let first = fixture.git(&["rev-parse", "HEAD"]);
    fixture.commit("source conflict", "source\n");
    let second = fixture.git(&["rev-parse", "HEAD"]);
    fixture.git(&["checkout", "-q", "main"]);
    fixture.commit("main conflict", "main\n");
    let before = fixture.refs();

    let (code, out, err) = fixture.rehearse(&["--keep", "cherry-pick", &first, &second]);
    assert_eq!(code, STOPPED, "{out}\n{err}");
    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();
    let sandbox = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .into_iter()
        .find(|sandbox| sandbox.id() == id)
        .expect("the kept sandbox exists");
    let sandbox_root = sandbox.root().to_owned();

    let (code, _, err) = fixture.rehearse(&["apply", &id]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("nothing that can be applied"), "{err}");
    assert_eq!(fixture.refs(), before, "the real repository is unchanged");
    assert!(
        !fixture.repo().join(".git/rehearse-undo").exists(),
        "a refused apply does not write an undo record"
    );
    assert!(
        sandbox_root.exists(),
        "the stopped sandbox remains available"
    );
}

#[test]
fn applying_a_kept_failed_rehearsal_is_refused_and_keeps_its_sandbox() {
    let fixture = Fixture::new();
    let before = fixture.refs();

    let (code, out, err) = fixture.rehearse(&["--keep", "merge", "no-such-branch"]);
    assert_eq!(code, FAILED, "{out}\n{err}");
    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();
    let sandbox = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .into_iter()
        .find(|sandbox| sandbox.id() == id)
        .expect("the kept sandbox exists");
    let sandbox_root = sandbox.root().to_owned();

    let (code, _, err) = fixture.rehearse(&["apply", &id]);

    assert_eq!(code, REFUSED, "{err}");
    assert!(
        err.contains("failed") && err.contains("nothing that can be applied"),
        "{err}"
    );
    assert_eq!(fixture.refs(), before, "the real repository is unchanged");
    assert!(
        sandbox_root.exists(),
        "the failed sandbox remains available"
    );
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
fn undo_puts_the_last_apply_back_and_then_has_nothing_left_to_do() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.git(&["rev-parse", "main"]);

    let (code, _, err) = fixture.rehearse(&["--apply", "merge", "--no-edit", "feature"]);
    assert_eq!(code, CLEAN, "{err}");
    assert_ne!(fixture.git(&["rev-parse", "main"]), before);

    let (code, out, err) = fixture.rehearse(&["undo"]);

    assert_eq!(code, CLEAN, "{err}");
    assert!(out.contains("put back:"), "{out}");
    assert!(out.contains("refs/heads/main"), "{out}");
    assert!(
        out.contains("from the apply of rehearsal"),
        "an undo with no prompt has to say which apply it took back: {out}"
    );
    assert_eq!(fixture.git(&["rev-parse", "main"]), before);

    // Consumed, so the second one is a clean nothing rather than a repeat.
    let (code, _, err) = fixture.rehearse(&["undo"]);
    assert_eq!(code, REFUSED, "{err}");
    assert!(err.contains("nothing to undo"), "{err}");
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

/// Every `git log --graph` the run spawned, counted off git's own trace.
fn graph_walks(trace: &str) -> usize {
    trace
        .lines()
        .filter(|line| line.contains("log --graph"))
        .count()
}

#[test]
fn stat_only_does_not_spawn_the_graph_walks_it_leaves_out() {
    // The point of the flag is the processes it does not start, not the
    // shorter output: an implementation that walked the history and then
    // dropped the result would satisfy every assertion about the report and
    // none about the wait. So the trace is the assertion.
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");

    let (code, full, _, full_trace) =
        fixture.rehearse_traced("trace-full", &["merge", "--no-edit", "feature"]);
    assert_eq!(code, CLEAN);
    let (code, short, _, short_trace) = fixture.rehearse_traced(
        "trace-stat",
        &["--stat-only", "merge", "--no-edit", "feature"],
    );
    assert_eq!(code, CLEAN, "{short}");

    assert!(
        graph_walks(&full_trace) > 0,
        "the full report walks the history: {full_trace}"
    );
    assert_eq!(
        graph_walks(&short_trace),
        0,
        "and --stat-only walks none of it: {short_trace}"
    );

    // What is left is everything that says what happened.
    assert!(!short.contains("graph  "), "{short}");
    assert!(full.contains("graph  refs/heads/main"), "{full}");
    assert!(
        short.contains("rehearsed  git merge --no-edit feature"),
        "{short}"
    );
    assert!(short.contains("refs/heads/main"), "{short}");
}

#[test]
fn stat_only_is_a_faster_report_and_never_a_weaker_check() {
    // The trap this guards against: "optimising" the flag further by skipping
    // the analysis. The drift check is what catches a rebase quietly changing
    // what a commit does, and it is exactly what the short report is read for.
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (_, out, _) = fixture.rehearse(&["--keep", "rebase", "main"]);
    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();

    // Resolve to something neither side said: content drift, the loud case.
    let sandbox = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .pop()
        .expect("the kept rehearsal is there");
    let worktree = sandbox.worktree();
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    let (code, _, err) = fixture.rehearse(&["--keep", "--stat-only", "continue", &id]);
    assert_eq!(code, CLEAN, "{err}");

    let (code, shown, _, trace) =
        fixture.rehearse_traced("trace-show", &["--stat-only", "show", &id]);

    assert_eq!(code, CLEAN, "{shown}");
    assert_eq!(graph_walks(&trace), 0, "{trace}");
    assert!(
        trace.contains("range-diff"),
        "the drift check still runs — it is the stat the short report is for: {trace}"
    );
    assert!(
        shown.contains("warning: content drift on refs/heads/feature"),
        "{shown}"
    );
    assert!(shown.contains("M file.txt"), "{shown}");
    assert!(!shown.contains("graph  "), "{shown}");
}

#[test]
fn stat_only_alongside_json_is_a_no_op_rather_than_a_refusal() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");

    let (code, document, err) =
        fixture.rehearse(&["--json", "--stat-only", "merge", "--no-edit", "feature"]);

    assert_eq!(code, CLEAN, "{err}");
    // Still one document, and the same one: the JSON report never carried the
    // graphs, so there is nothing for the flag to leave out.
    let (_, plain, _) = fixture.rehearse(&["--json", "merge", "--no-edit", "feature"]);
    assert_eq!(normalised(&document), normalised(&plain), "{document}");

    // And --help says so, because a flag that silently does nothing is a bug
    // report waiting to be filed.
    let (_, help, _) = fixture.rehearse(&["--help"]);
    assert!(help.contains("--stat-only"), "{help}");
    assert!(
        help.contains("with --json it does nothing at all"),
        "{help}"
    );
}

/// One JSON report with everything two separate runs are entitled to disagree
/// about removed: the rehearsal id, its sandbox path, and the commit the merge
/// produced (a merge commit carries the wall clock, so no two runs write the
/// same one). What is left differs only if the flag changed the document.
fn normalised(text: &str) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(text).expect("one JSON document");
    let object = value.as_object_mut().expect("the document is an object");
    object.remove("id");
    object.remove("sandbox");
    if let Some(refs) = object
        .get_mut("refs")
        .and_then(serde_json::Value::as_array_mut)
    {
        for entry in refs {
            if let Some(entry) = entry.as_object_mut() {
                entry.remove("after");
            }
        }
    }
    value
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
    assert!(help.contains("git rehearse undo"), "{help}");
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
fn a_stopped_rehearsal_survives_having_nobody_to_ask_so_its_own_directions_work() {
    // The regression this guards: without --keep, a stopped rehearsal printed
    // its sandbox path and `git rehearse continue <id>`, then deleted the
    // sandbox three lines later. Every instruction was already false by the
    // time it was read — and since an agent never has a terminal, that was the
    // path v2's whole audience takes.
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let before = fixture.refs();

    // No --keep, no --apply, and the harness gives the process no terminal.
    let (code, out, err) = fixture.rehearse(&["rebase", "main"]);
    assert_eq!(code, STOPPED, "{err}");

    assert!(
        out.contains("the rehearsal was kept"),
        "the skipped question still has to be announced, with the answer it got: {out}"
    );
    assert!(
        !out.contains("answer [k]eep below"),
        "there is no prompt below when there is no terminal: {out}"
    );

    let id = out
        .lines()
        .find_map(|line| line.strip_prefix("rehearsal  "))
        .expect("the report names the rehearsal")
        .to_owned();
    let sandbox = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .pop()
        .expect("a stopped rehearsal is kept rather than discarded");
    let worktree = sandbox.worktree();
    assert!(
        out.contains(&worktree.display().to_string()),
        "the directions name a path: {out}"
    );

    // The point of keeping it: every instruction it printed can be carried out.
    assert!(worktree.is_dir(), "and that path still exists");
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    let (code, cont, err) = fixture.rehearse(&["--keep", "continue", &id]);

    assert_eq!(code, CLEAN, "{err}\n{cont}");
    assert_eq!(
        fixture.refs(),
        before,
        "and none of it touched the real repository"
    );
}

#[test]
fn a_refused_apply_does_not_walk_away_from_its_sandbox() {
    // The regression: the refusal returned before anything disposed of the
    // rehearsal, so it sat in the cache as `Fresh` for the full TTL — a state
    // no code path deliberately creates, and one `list` then displayed.
    let fixture = Fixture::new();

    // Git refused the command outright: nothing in progress, nothing to return
    // to, so it goes.
    let (code, _, err) = fixture.rehearse(&["--apply", "merge", "no-such-branch"]);
    assert_eq!(code, REFUSED, "{err}");
    assert!(
        sandbox::list(fixture.cache(), None)
            .expect("the cache lists")
            .is_empty(),
        "a failed rehearsal leaves nothing worth keeping"
    );

    // A stopped one is the opposite: its sandbox is the only copy of where the
    // command got to, so the refusal keeps it and says where it went.
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (code, _, err) = fixture.rehearse(&["--apply", "rebase", "main"]);

    assert_eq!(code, REFUSED, "{err}");
    let kept = sandbox::list(fixture.cache(), None)
        .expect("the cache lists")
        .pop()
        .expect("a stopped rehearsal survives a refused --apply");
    assert_eq!(
        kept.meta().status,
        sandbox::Status::Kept,
        "and it is Kept, not left Fresh"
    );
    assert!(
        err.contains(kept.id()) && err.contains("git rehearse continue"),
        "the refusal has to say where it went: {err}"
    );
}

#[test]
fn a_clean_rehearsal_is_still_discarded_when_there_is_nobody_to_ask() {
    // The other half of the rule, kept honest: only a *stopped* rehearsal is
    // worth keeping unasked. A clean one nobody claimed can be had again by
    // running the command again, so a scripted loop must not accumulate them.
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");

    let (code, out, err) = fixture.rehearse(&["merge", "--no-edit", "feature"]);

    assert_eq!(code, CLEAN, "{err}");
    assert!(out.contains("the rehearsal was discarded"), "{out}");
    assert!(
        sandbox::list(fixture.cache(), None)
            .expect("the cache lists")
            .is_empty(),
        "nothing left behind: {out}"
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
