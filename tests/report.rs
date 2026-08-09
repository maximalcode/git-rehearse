//! The report, rendered from real rehearsals.
//!
//! The unit tests in `report` cover the wording against handmade data; these
//! cover the other half — that a real sandbox produces data the report can
//! describe, and that the graph really is the affected subgraph rather than
//! the whole history.

mod support;

use git_rehearse::sandbox::Plan;
use git_rehearse::{analyze, execute, preflight, report, sandbox};
use support::Fixture;

const NOW: u64 = 1_786_248_000;

fn plan_of(fixture: &Fixture, command: &[&str]) -> Plan {
    preflight::run(fixture.repo())
        .expect("the fixture passes preflight")
        .into_plan(command.iter().map(|arg| (*arg).to_owned()).collect())
}

#[test]
fn a_real_merge_renders_a_report_with_a_before_and_after_graph() {
    let fixture = Fixture::new();
    // main moves on in a file `feature` never touched, so the branches diverge
    // without conflicting: a merge that actually merges, and a graph worth
    // drawing.
    fixture.commit_file("other.txt", "other\n", "four");
    let plan = plan_of(&fixture, &["merge", "--no-edit", "feature"]);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox");
    let worktree = sandbox.worktree();

    let outcome = execute::run(&worktree, &plan.command, None).expect("git runs");
    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &outcome)
        .expect("the sandbox can be read");
    let graphs = report::graphs(&worktree, &analysis, report::Detail::Full).expect("graphs render");
    let text = report::render(sandbox.meta(), &analysis, &outcome, &graphs);

    assert!(
        text.contains("rehearsed  git merge --no-edit feature"),
        "{text}"
    );
    assert!(text.contains("refs/heads/main"), "{text}");
    assert!(text.contains("graph  refs/heads/main"), "{text}");
    assert!(text.contains("  before"), "{text}");
    assert!(text.contains("  after"), "{text}");
    // The merge commit git wrote is in the "after" side and not the "before".
    let (before, after) = text
        .split_once("  after")
        .expect("the report has both halves");
    assert!(after.contains("Merge branch"), "{after}");
    assert!(!before.contains("Merge branch"), "{before}");
    assert!(
        !text.contains("warning:"),
        "a merge is allowed to change content: {text}"
    );
}

#[test]
fn the_graph_covers_the_affected_subgraph_and_not_the_whole_history() {
    let fixture = Fixture::new();
    // Ten commits of history nobody needs to see again, in files `feature`
    // never touched.
    for n in 0..10 {
        fixture.commit_file(&format!("noise{n}.txt"), "noise\n", &format!("noise {n}"));
    }
    let plan = plan_of(&fixture, &["merge", "--no-edit", "feature"]);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox");
    let worktree = sandbox.worktree();

    let outcome = execute::run(&worktree, &plan.command, None).expect("git runs");
    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &outcome)
        .expect("the sandbox can be read");
    let graphs = report::graphs(&worktree, &analysis, report::Detail::Full).expect("graphs render");

    let drawn = graphs
        .iter()
        .map(|graph| graph.before.lines().count() + graph.after.lines().count())
        .sum::<usize>();
    assert!(
        drawn < 10,
        "the graph should show what changed, not the whole history: {graphs:?}"
    );
    assert!(
        !graphs.iter().any(|graph| graph.reference == "HEAD"),
        "HEAD moves with its branch; drawing it twice says nothing new"
    );
}

#[test]
fn stat_only_leaves_out_the_graphs_and_nothing_else_the_report_says() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let plan = plan_of(&fixture, &["merge", "--no-edit", "feature"]);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox");
    let worktree = sandbox.worktree();

    let outcome = execute::run(&worktree, &plan.command, None).expect("git runs");
    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &outcome)
        .expect("the sandbox can be read");

    let full = report::graphs(&worktree, &analysis, report::Detail::Full).expect("graphs render");
    let short =
        report::graphs(&worktree, &analysis, report::Detail::StatOnly).expect("nothing renders");

    assert!(!full.is_empty(), "there is a graph to leave out");
    assert!(short.is_empty(), "{short:?}");

    let text = report::render(sandbox.meta(), &analysis, &outcome, &short);
    assert!(!text.contains("graph  "), "{text}");
    // The analysis behind the report is untouched, so everything it produced
    // is still here — the ref moves the reader came for above all.
    assert!(
        text.contains("rehearsed  git merge --no-edit feature"),
        "{text}"
    );
    assert!(text.contains("refs/heads/main"), "{text}");
}

#[test]
fn a_rebase_whose_resolution_changed_the_content_reports_the_warning() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let plan = plan_of(&fixture, &["rebase", "main"]);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox");
    let worktree = sandbox.worktree();

    execute::run(&worktree, &plan.command, None).expect("git runs");
    // Resolve to something neither side said, then finish the rebase.
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);

    let outcome = execute::Outcome::Clean;
    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &outcome)
        .expect("the sandbox can be read");
    let graphs = report::graphs(&worktree, &analysis, report::Detail::Full).expect("graphs render");
    let text = report::render(sandbox.meta(), &analysis, &outcome, &graphs);

    assert!(
        text.contains("warning: content drift on refs/heads/feature"),
        "{text}"
    );
    assert!(text.contains("M file.txt"), "{text}");
    assert!(report::can_apply(&analysis, &outcome), "{text}");
}

#[test]
fn a_stopped_rehearsal_is_not_offered_for_applying() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let plan = plan_of(&fixture, &["rebase", "main"]);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox");
    let worktree = sandbox.worktree();

    let outcome = execute::run(&worktree, &plan.command, None).expect("git runs");
    let analysis = analyze::run(&worktree, &plan.pre_state, &plan.command, &outcome)
        .expect("the sandbox can be read");
    let text = report::render(sandbox.meta(), &analysis, &outcome, &[]);

    assert!(text.contains("stopped part-way on a conflict"), "{text}");
    assert!(text.contains("file.txt  1 hunk"), "{text}");
    assert!(
        !report::can_apply(&analysis, &outcome),
        "half of a rebase is not a result anybody inspected"
    );
}
