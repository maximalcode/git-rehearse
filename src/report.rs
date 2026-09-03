//! The terminal output, and the question at the end of it.
//!
//! This is the product. Everything else in the crate exists so that this text
//! can be true; the text is what decides whether someone trusts the answer
//! enough to apply it.
//!
//! Written like git's own output: lowercase section headings, no colour, no
//! decoration, `warning:` where git would say `warning:`. A rehearsal report
//! appears in the middle of a working session, next to real git output, and
//! anything more theatrical would read as a different tool shouting.

use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::analyze::{Analysis, RefMove};
use crate::carry::{Carry, Replay};
use crate::execute::Outcome;
use crate::sandbox::Meta;
use crate::{Result, carry, git};

/// How many characters of a commit id the report shows.
const SHORT: usize = 8;

/// A before/after view of one rewritten ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Graph {
    /// The ref the two graphs describe.
    pub reference: String,
    /// `git log --graph` of the old tip, back to the common ancestor.
    pub before: String,
    /// The same, for the new tip.
    pub after: String,
}

/// How much of the report gets drawn.
///
/// **This skips rendering, never analysis — and that is the whole design.**
/// [`graphs`] is the only thing `StatOnly` switches off: two
/// `git log --graph` processes per moved ref, plus a fallback walk whenever the
/// bounded range comes back empty. Everything [`crate::analyze::run`] does still
/// runs in full, `git range-diff` drift detection included.
///
/// The temptation to go further is obvious — `range-diff` is the larger cost on
/// a big rehearsal — and it is a trap. Drift detection is what catches a rebase
/// quietly changing what a commit does, so a fast mode that turned it off would
/// be trading away the one check that justifies the tool for a shorter wait. The
/// flag is called `stat-only`, not `unchecked`; the drift stat is *in* the fast
/// output, so the analysis has to run to produce it either way. Skipping it
/// would also mean `--stat-only --json` emitting a document with fields missing,
/// which the schema has no way to express.
///
/// So: if you are here to make `--stat-only` faster, the answer is not to
/// analyse less. It is to make the analysis cheaper for both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Detail {
    /// The whole report, before/after graphs included.
    #[default]
    Full,
    /// Everything that says *what happened*, nothing that draws *where you
    /// were*: header, ref moves, carried work, conflicts, drift.
    StatOnly,
}

impl Detail {
    /// Whether the before/after graphs are worth the processes they cost.
    #[must_use]
    pub fn wants_graphs(self) -> bool {
        self == Self::Full
    }
}

/// What the user decided to do with a rehearsal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Move the real repository's refs to the rehearsed commits.
    Apply,
    /// Delete the sandbox now.
    Discard,
    /// Leave it in the cache to look at later.
    Keep,
}

/// Builds the before/after graphs for every rewritten ref.
///
/// SCOPE.md's open question 2 asked whether to hand-roll ASCII graph rendering
/// or delegate to `git log --graph`, and marked delegation as the sanctioned
/// v1 shortcut. Delegating: it is the drawing the user already reads every
/// day, it handles octopus merges and criss-cross history that a hand-rolled
/// renderer would get subtly wrong, and it costs nothing to maintain.
///
/// Only the affected subgraph is drawn — each side back to the common
/// ancestor of the old and new tip, plus the boundary commit for context.
/// Rendering the whole history would bury the three commits that changed.
///
/// This is also the one place [`Detail`] is honoured, deliberately: the graphs
/// are the entire cost `--stat-only` avoids, and a single guard here means no
/// caller can spawn them by forgetting. [`render`] draws no graph section for an
/// empty slice, so the rest of the report needs to know nothing about the flag.
///
/// # Errors
///
/// [`Error::Git`](crate::Error::Git) if the sandbox cannot be read.
pub fn graphs(worktree: &Path, analysis: &Analysis, detail: Detail) -> Result<Vec<Graph>> {
    if !detail.wants_graphs() {
        return Ok(Vec::new());
    }
    let mut graphs = Vec::new();
    for moved in &analysis.ref_moves {
        // HEAD moves with its branch; drawing it twice says nothing new.
        if moved.name == HEAD_NAME {
            continue;
        }
        let (Some(before), Some(after)) = (&moved.before, &moved.after) else {
            continue;
        };
        graphs.push(Graph {
            reference: moved.name.clone(),
            before: log_graph(worktree, before, after)?,
            after: log_graph(worktree, after, before)?,
        });
    }
    Ok(graphs)
}

/// `git log --graph` for `tip`, stopping where it rejoins `other`.
///
/// The bounded range is empty whenever one tip is an ancestor of the other —
/// a fast-forward, or a merge whose old tip the new one contains — and an
/// empty "before" block tells the reader nothing about where they were. So an
/// empty range, or one git will not compute because the two sides share no
/// history, falls back to the last few commits from the tip.
fn log_graph(worktree: &Path, tip: &str, other: &str) -> Result<String> {
    let bounded = git::run(
        worktree,
        [
            "log",
            "--graph",
            "--oneline",
            "--decorate",
            "--boundary",
            // Everything reachable from this tip but not from the other side:
            // the affected subgraph, and nothing else.
            tip,
            &format!("^{other}"),
        ],
    );
    match bounded {
        Ok(graph) if !graph.trim().is_empty() => Ok(graph),
        _ => git::run(
            worktree,
            ["log", "--graph", "--oneline", "--decorate", "-3", tip],
        ),
    }
}

/// The pre-state key for `HEAD`, repeated here to keep the module's imports
/// to the ones it actually reasons about.
const HEAD_NAME: &str = "HEAD";

/// Renders the whole report.
///
/// Pure: everything it needs has already been read out of the sandbox, so the
/// wording is testable without a repository — which matters, because the
/// wording is the part users actually judge.
#[must_use]
pub fn render(meta: &Meta, analysis: &Analysis, outcome: &Outcome, graphs: &[Graph]) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "rehearsed  git {}", meta.command.join(" "));
    let _ = writeln!(out, "repository {}", meta.repo_path.display());
    let _ = writeln!(out, "rehearsal  {}", meta.id);
    let _ = writeln!(out);

    match outcome {
        Outcome::Clean => {}
        // The command finished; it is putting the uncommitted work back that
        // stopped. Saying "the command stopped" here would send someone
        // looking for a half-finished rebase that is not there.
        Outcome::Stopped { .. } if carry::stopped_on_replay(meta) => {
            let _ = writeln!(
                out,
                "the command ran, but your uncommitted changes did not go back on"
            );
            let _ = writeln!(out);
        }
        Outcome::Stopped { conflicts } => {
            let _ = writeln!(
                out,
                "the command stopped part-way{}",
                if *conflicts {
                    " on a conflict"
                } else {
                    " (nothing is unmerged — an edit or break in the todo)"
                }
            );
            let _ = writeln!(out);
        }
        Outcome::Failed { code } => {
            let _ = writeln!(
                out,
                "git refused the command{}",
                code.map_or(String::new(), |code| format!(" (exit {code})"))
            );
            let _ = writeln!(out);
        }
    }

    render_refs(&mut out, &analysis.ref_moves);
    render_carried(&mut out, meta.carry.as_ref());
    render_conflicts(&mut out, analysis);
    render_graphs(&mut out, graphs);
    render_drift(&mut out, analysis);

    out
}

/// What was carried through the rehearsal, and whether it came back.
///
/// The second half of the question, and the reason this feature exists: a
/// report that says the rebase is fine and nothing about the work in your
/// worktree has answered the easy half.
fn render_carried(out: &mut String, carry: Option<&Carry>) {
    let Some(carry) = carry else {
        return;
    };
    let _ = writeln!(
        out,
        "carried  {} uncommitted path(s): {}",
        carry.paths.len(),
        carry::describe(&carry.paths)
    );
    match &carry.replay {
        None => {
            let _ = writeln!(
                out,
                "  not put back — the command did not finish, so there was nothing to put them on"
            );
        }
        Some(Replay::Restored { result: Some(_) }) => {
            let _ = writeln!(out, "  they come back clean on the rehearsed history");
        }
        Some(Replay::Restored { result: None }) => {
            let _ = writeln!(
                out,
                "  nothing comes back — the rehearsed history already contains these changes"
            );
        }
        Some(Replay::Conflicted { paths }) => {
            let _ = writeln!(
                out,
                "  they do NOT come back clean — {} path(s) conflict in the sandbox:",
                paths.len()
            );
            for path in paths {
                let _ = writeln!(out, "    {path}");
            }
        }
        Some(Replay::Refused { reason }) => {
            let _ = writeln!(out, "  they could not be put back: {reason}");
        }
        Some(Replay::NotNeeded) => {
            let _ = writeln!(
                out,
                "  they stay where they are — this rehearsal does not move your worktree"
            );
        }
    }
    let _ = writeln!(out);
}

fn render_refs(out: &mut String, moves: &[RefMove]) {
    if moves.is_empty() {
        let _ = writeln!(out, "refs\n  nothing moved");
        let _ = writeln!(out);
        return;
    }
    let width = moves
        .iter()
        .map(|moved| moved.name.len())
        .max()
        .unwrap_or_default();
    let _ = writeln!(out, "refs");
    for moved in moves {
        let _ = writeln!(
            out,
            "  {:width$}  {}",
            moved.name,
            describe_move(moved),
            width = width
        );
    }
    let _ = writeln!(out);
}

/// One ref's movement, as a phrase.
fn describe_move(moved: &RefMove) -> String {
    match (&moved.before, &moved.after) {
        (Some(before), Some(after)) => format!("{} -> {}", short(before), short(after)),
        (None, Some(after)) => format!("created at {}", short(after)),
        (Some(before), None) => format!("deleted (was {})", short(before)),
        // Not reachable: a ref that exists on neither side never moved.
        (None, None) => "unchanged".to_owned(),
    }
}

fn render_conflicts(out: &mut String, analysis: &Analysis) {
    if analysis.conflicts.is_empty() && analysis.stopped_at.is_none() {
        return;
    }
    match &analysis.stopped_at {
        Some(commit) => {
            let _ = writeln!(
                out,
                "conflicts  stopped at {} \"{}\"",
                short(&commit.sha),
                commit.subject
            );
        }
        None => {
            let _ = writeln!(out, "conflicts");
        }
    }
    for conflict in &analysis.conflicts {
        let hunks = match conflict.hunks {
            0 => "no text hunks (binary, or add/add)".to_owned(),
            1 => "1 hunk".to_owned(),
            many => format!("{many} hunks"),
        };
        let _ = writeln!(out, "  {}  {hunks}", conflict.path);
    }
    let _ = writeln!(out);
}

fn render_graphs(out: &mut String, graphs: &[Graph]) {
    for graph in graphs {
        let _ = writeln!(out, "graph  {}", graph.reference);
        let _ = writeln!(out, "  before");
        indent_into(out, &graph.before);
        let _ = writeln!(out, "  after");
        indent_into(out, &graph.after);
        let _ = writeln!(out);
    }
}

fn indent_into(out: &mut String, block: &str) {
    for line in block.lines() {
        let _ = writeln!(out, "    {line}");
    }
}

fn render_drift(out: &mut String, analysis: &Analysis) {
    for drift in &analysis.drift {
        let replay = &drift.replay;
        if analysis.drift_expected_empty && replay.is_suspicious() {
            // The loud one. This is the line the tool exists to print.
            let _ = writeln!(out, "warning: content drift on {}", drift.reference);
            if replay.compared {
                let _ = writeln!(
                    out,
                    "  replaying a commit should not change what it does. These did change:"
                );
                for subject in &replay.changed {
                    let _ = writeln!(out, "    changed  {subject}");
                }
                for subject in &replay.dropped {
                    let _ = writeln!(out, "    gone     {subject}");
                }
                let _ = writeln!(
                    out,
                    "  A conflict resolution, a merge driver or a dropped commit did this."
                );
            } else {
                let _ = writeln!(
                    out,
                    "  the content differs and the old and new commits could not be compared,"
                );
                let _ = writeln!(
                    out,
                    "  so what changed cannot be narrowed down. Read the diff."
                );
            }
        } else if analysis.drift_expected_empty {
            // Content differs, and every replayed commit still does what it
            // did — so the difference came from the base, which is what
            // rebasing onto a moved base is *for*. Said once, calmly.
            let _ = writeln!(
                out,
                "content  {} — every replayed commit is unchanged; the difference is the {} \
                 commit(s) picked up from the new base",
                drift.reference,
                replay.added.len()
            );
        } else {
            let _ = writeln!(out, "content  {}", drift.reference);
        }
        for change in &drift.changes {
            let _ = writeln!(out, "    {} {}", change.status, change.path);
        }
        let _ = writeln!(out);
    }
}

/// Abbreviates a commit id for display.
fn short(sha: &str) -> &str {
    sha.get(..SHORT).unwrap_or(sha)
}

/// Whether applying makes sense for this rehearsal.
///
/// A stopped or failed command leaves the sandbox mid-operation: its refs are
/// not a result anybody inspected, so offering to transplant them would be
/// offering nonsense. Resolving inside the sandbox and then applying is v1.x.
#[must_use]
pub fn can_apply(analysis: &Analysis, outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Clean) && !analysis.ref_moves.is_empty()
}

/// What to do when nobody can be asked.
///
/// Never apply: nothing is transplanted into a real repository without somebody
/// saying so. Beyond that the answer depends on whether the rehearsal left
/// anything worth coming back to.
///
/// **Clean, or failed → discard.** SCOPE.md's reasoning holds: a script running
/// rehearsals in a loop must not silently fill the user's cache. A clean
/// rehearsal nobody claimed can be reproduced by running it again, and a failed
/// one left nothing in progress at all.
///
/// **Stopped → keep.** Here that reasoning inverts. A stopped rehearsal *is*
/// its sandbox — a real repository sitting mid-rebase with the conflict in it —
/// and that is the case this tool exists for. Discarding it deletes the very
/// thing the report just gave directions to, and the directions with it: the
/// path, the id, and the `git rehearse continue` that #38 was built for. Since
/// [`can_apply`] is already false for a stopped rehearsal, keeping is the only
/// answer here that destroys nothing, and the seven-day TTL still collects it.
#[must_use]
pub fn non_interactive(outcome: &Outcome) -> Choice {
    match outcome {
        Outcome::Stopped { .. } => Choice::Keep,
        Outcome::Clean | Outcome::Failed { .. } => Choice::Discard,
    }
}

/// Asks what to do with the rehearsal.
///
/// Generic over the streams so the loop is testable without a terminal — the
/// same reason the rest of this module is pure.
///
/// There is deliberately **no default on Enter**: discarding a rehearsal
/// somebody wanted, or keeping one they did not, are both worse than asking
/// again. End of input is a different matter — that is not a person, and it
/// means discard.
///
/// # Errors
///
/// Whatever the streams return.
pub fn ask<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    can_apply: bool,
) -> io::Result<Choice> {
    let options = if can_apply {
        "[a]pply / [d]iscard / [k]eep"
    } else {
        "[d]iscard / [k]eep"
    };
    loop {
        write!(output, "{options}? ")?;
        output.flush()?;

        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            // EOF: the caller redirected stdin or the pipe closed.
            writeln!(output)?;
            return Ok(Choice::Discard);
        }
        match parse(answer.trim(), can_apply) {
            Some(choice) => return Ok(choice),
            None => writeln!(output, "please answer with one of {options}")?,
        }
    }
}

/// Parses one answer.
fn parse(answer: &str, can_apply: bool) -> Option<Choice> {
    match answer.to_ascii_lowercase().as_str() {
        "a" | "apply" if can_apply => Some(Choice::Apply),
        "d" | "discard" => Some(Choice::Discard),
        "k" | "keep" => Some(Choice::Keep),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Choice, Detail, Graph, ask, can_apply, graphs, non_interactive, parse, render, short,
    };
    use crate::analyze::{Analysis, Commit, Conflict, Drift, FileChange, RefMove, Replay};
    // Two different replays meet in this file: what happened to the *commits*
    // (above) and what happened to the *carried changes* (below).
    use crate::carry::{Carry, Replay as Carried};
    use crate::execute::Outcome;
    use crate::sandbox::{Checkout, META_SCHEMA, Meta, Status};
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    fn meta(command: &[&str]) -> Meta {
        Meta {
            schema: META_SCHEMA,
            id: "1786248000-00".to_owned(),
            repo_id: "app-0123456789abcdef".to_owned(),
            repo_path: PathBuf::from("/repos/app"),
            command: command.iter().map(|arg| (*arg).to_owned()).collect(),
            checkout: Checkout::Branch("feature".to_owned()),
            pre_state: BTreeMap::new(),
            carry: None,
            created_unix: 1_786_248_000,
            status: Status::Fresh,
            result: None,
        }
    }

    /// The same, with uncommitted work carried through it.
    fn meta_carrying(command: &[&str], replay: Option<Carried>) -> Meta {
        Meta {
            carry: Some(Carry {
                snapshot: "5ea51a5h".to_owned(),
                paths: vec!["notes.txt".to_owned(), "src/main.rs".to_owned()],
                replay,
            }),
            ..meta(command)
        }
    }

    fn moved(name: &str) -> RefMove {
        RefMove {
            name: name.to_owned(),
            before: Some("aaaaaaaaaaaa1111".to_owned()),
            after: Some("bbbbbbbbbbbb2222".to_owned()),
        }
    }

    fn analysis() -> Analysis {
        Analysis {
            ref_moves: vec![moved("refs/heads/feature")],
            stopped_at: None,
            conflicts: Vec::new(),
            drift: Vec::new(),
            drift_expected_empty: true,
        }
    }

    #[test]
    fn the_header_says_what_was_rehearsed_and_where() {
        let report = render(
            &meta(&["rebase", "-i", "main"]),
            &analysis(),
            &Outcome::Clean,
            &[],
        );
        assert!(report.contains("rehearsed  git rebase -i main"), "{report}");
        assert!(report.contains("/repos/app"), "{report}");
        assert!(report.contains("1786248000-00"), "{report}");
    }

    #[test]
    fn ref_moves_are_shown_old_to_new_and_abbreviated() {
        let report = render(&meta(&["merge", "x"]), &analysis(), &Outcome::Clean, &[]);
        assert!(
            report.contains("refs/heads/feature  aaaaaaaa -> bbbbbbbb"),
            "{report}"
        );
    }

    #[test]
    fn a_created_or_deleted_branch_reads_as_such() {
        let mut analysis = analysis();
        analysis.ref_moves = vec![
            RefMove {
                name: "refs/heads/new".to_owned(),
                before: None,
                after: Some("cccccccccccc".to_owned()),
            },
            RefMove {
                name: "refs/heads/gone".to_owned(),
                before: Some("dddddddddddd".to_owned()),
                after: None,
            },
        ];
        let report = render(&meta(&["merge", "x"]), &analysis, &Outcome::Clean, &[]);
        assert!(report.contains("created at cccccccc"), "{report}");
        assert!(report.contains("deleted (was dddddddd)"), "{report}");
    }

    #[test]
    fn a_rehearsal_that_moved_nothing_says_so_rather_than_showing_an_empty_heading() {
        let mut analysis = analysis();
        analysis.ref_moves.clear();
        let report = render(&meta(&["merge", "main"]), &analysis, &Outcome::Clean, &[]);
        assert!(report.contains("nothing moved"), "{report}");
    }

    /// A drift entry whose replay says what `replay` says.
    fn drift(replay: Replay) -> Drift {
        Drift {
            reference: "refs/heads/feature".to_owned(),
            changes: vec![FileChange {
                status: "M".to_owned(),
                path: "src/main.rs".to_owned(),
            }],
            commits_before: 1,
            commits_after: 2,
            replay,
        }
    }

    #[test]
    fn a_commit_that_changed_when_replayed_is_a_warning_that_explains_itself() {
        let mut analysis = analysis();
        analysis.drift = vec![drift(Replay {
            changed: vec!["teach the parser about tabs".to_owned()],
            compared: true,
            added: vec!["main moves on".to_owned()],
            ..Replay::default()
        })];
        let report = render(&meta(&["rebase", "main"]), &analysis, &Outcome::Clean, &[]);

        assert!(report.contains("warning: content drift"), "{report}");
        assert!(
            report.contains("changed  teach the parser about tabs"),
            "{report}"
        );
        assert!(report.contains("M src/main.rs"), "{report}");
        // The explanation matters as much as the warning: a warning nobody
        // understands gets ignored the second time.
        assert!(report.contains("conflict resolution"), "{report}");
    }

    #[test]
    fn a_dropped_commit_is_named_rather_than_left_to_be_noticed() {
        let mut analysis = analysis();
        analysis.drift = vec![drift(Replay {
            dropped: vec!["fix the off-by-one".to_owned()],
            compared: true,
            ..Replay::default()
        })];
        let report = render(
            &meta(&["rebase", "-i", "main"]),
            &analysis,
            &Outcome::Clean,
            &[],
        );

        assert!(report.contains("warning: content drift"), "{report}");
        assert!(report.contains("gone     fix the off-by-one"), "{report}");
    }

    #[test]
    fn content_that_came_from_the_new_base_is_not_a_warning() {
        // The most ordinary rebase there is: `rebase main` where main has
        // moved on. The tip's tree differs by exactly the base's own commits,
        // and warning about that would train people to ignore the warning.
        let mut analysis = analysis();
        analysis.drift = vec![drift(Replay {
            added: vec!["a".to_owned(), "b".to_owned()],
            compared: true,
            ..Replay::default()
        })];
        let report = render(&meta(&["rebase", "main"]), &analysis, &Outcome::Clean, &[]);

        assert!(!report.contains("warning"), "{report}");
        assert!(
            report.contains("every replayed commit is unchanged"),
            "{report}"
        );
        assert!(
            report.contains("2 commit(s) picked up from the new base"),
            "{report}"
        );
        assert!(
            report.contains("M src/main.rs"),
            "still says what differs: {report}"
        );
    }

    #[test]
    fn a_rewrite_git_could_not_compare_is_treated_as_suspicious() {
        let mut analysis = analysis();
        analysis.drift = vec![drift(Replay::default())];
        let report = render(&meta(&["rebase", "main"]), &analysis, &Outcome::Clean, &[]);

        assert!(report.contains("warning: content drift"), "{report}");
        assert!(report.contains("could not be compared"), "{report}");
    }

    #[test]
    fn the_same_difference_after_a_merge_is_reported_without_alarm() {
        let mut analysis = analysis();
        analysis.drift_expected_empty = false;
        analysis.drift = vec![Drift {
            reference: "refs/heads/main".to_owned(),
            changes: vec![FileChange {
                status: "M".to_owned(),
                path: "src/main.rs".to_owned(),
            }],
            commits_before: 1,
            commits_after: 1,
            replay: Replay {
                compared: true,
                ..Replay::default()
            },
        }];
        let report = render(
            &meta(&["merge", "feature"]),
            &analysis,
            &Outcome::Clean,
            &[],
        );

        assert!(
            !report.contains("warning"),
            "a merge changes content: {report}"
        );
        assert!(report.contains("content  refs/heads/main"), "{report}");
        assert!(report.contains("M src/main.rs"), "{report}");
    }

    #[test]
    fn carried_work_that_comes_back_clean_says_so_in_one_line() {
        let report = render(
            &meta_carrying(
                &["rebase", "main"],
                Some(Carried::Restored {
                    result: Some("re914yed".to_owned()),
                }),
            ),
            &analysis(),
            &Outcome::Clean,
            &[],
        );
        assert!(
            report.contains("carried  2 uncommitted path(s): notes.txt, src/main.rs"),
            "{report}"
        );
        assert!(report.contains("come back clean"), "{report}");
    }

    #[test]
    fn carried_work_the_new_history_already_contains_is_not_silently_dropped() {
        // The changes are gone from the worktree after an apply, and the
        // reason is that they are in the commits now. Saying nothing here
        // would look exactly like losing them.
        let report = render(
            &meta_carrying(
                &["rebase", "main"],
                Some(Carried::Restored { result: None }),
            ),
            &analysis(),
            &Outcome::Clean,
            &[],
        );
        assert!(report.contains("nothing comes back"), "{report}");
        assert!(report.contains("already contains"), "{report}");
    }

    #[test]
    fn a_replay_that_conflicts_is_reported_as_the_command_having_run() {
        // The rebase worked. What stopped is putting the uncommitted work
        // back, and a report that said "the command stopped part-way" would
        // send someone looking for a half-finished rebase that is not there.
        let report = render(
            &meta_carrying(
                &["rebase", "main"],
                Some(Carried::Conflicted {
                    paths: vec!["notes.txt".to_owned()],
                }),
            ),
            &analysis(),
            &Outcome::Stopped { conflicts: true },
            &[],
        );
        assert!(
            report.contains("the command ran, but your uncommitted changes did not go back on"),
            "{report}"
        );
        assert!(!report.contains("the command stopped part-way"), "{report}");
        assert!(report.contains("do NOT come back clean"), "{report}");
        assert!(report.contains("    notes.txt"), "{report}");
    }

    #[test]
    fn a_replay_that_never_happened_says_why_rather_than_nothing() {
        let stopped = render(
            &meta_carrying(&["rebase", "main"], None),
            &analysis(),
            &Outcome::Stopped { conflicts: true },
            &[],
        );
        assert!(stopped.contains("not put back"), "{stopped}");
        // And the ordinary stopped-command wording is back, because this time
        // it is the command that stopped.
        assert!(
            stopped.contains("the command stopped part-way"),
            "{stopped}"
        );

        let blocked = render(
            &meta_carrying(
                &["--", "checkout", "-p"],
                Some(Carried::Refused {
                    reason: "error: your local changes would be overwritten".to_owned(),
                }),
            ),
            &analysis(),
            &Outcome::Stopped { conflicts: false },
            &[],
        );
        assert!(blocked.contains("could not be put back"), "{blocked}");
        assert!(blocked.contains("would be overwritten"), "{blocked}");
    }

    #[test]
    fn a_rehearsal_that_carried_nothing_has_no_carried_section_at_all() {
        let report = render(
            &meta(&["rebase", "main"]),
            &analysis(),
            &Outcome::Clean,
            &[],
        );
        assert!(!report.contains("carried"), "{report}");
    }

    #[test]
    fn a_stopped_command_names_the_commit_and_counts_the_hunks() {
        let mut analysis = analysis();
        analysis.stopped_at = Some(Commit {
            sha: "eeeeeeeeeeee3333".to_owned(),
            subject: "teach the parser about tabs".to_owned(),
        });
        analysis.conflicts = vec![
            Conflict {
                path: "src/parse.rs".to_owned(),
                hunks: 2,
            },
            Conflict {
                path: "logo.png".to_owned(),
                hunks: 0,
            },
        ];
        let report = render(
            &meta(&["rebase", "main"]),
            &analysis,
            &Outcome::Stopped { conflicts: true },
            &[],
        );

        assert!(
            report.contains("stopped part-way on a conflict"),
            "{report}"
        );
        assert!(
            report.contains("stopped at eeeeeeee \"teach the parser about tabs\""),
            "{report}"
        );
        assert!(report.contains("src/parse.rs  2 hunks"), "{report}");
        assert!(
            report.contains("logo.png  no text hunks (binary, or add/add)"),
            "one hunk and none are different problems: {report}"
        );
    }

    #[test]
    fn a_stop_without_conflicts_is_not_called_a_conflict() {
        let report = render(
            &meta(&["rebase", "-i", "main"]),
            &analysis(),
            &Outcome::Stopped { conflicts: false },
            &[],
        );
        assert!(report.contains("edit or break"), "{report}");
        assert!(!report.contains("on a conflict"), "{report}");
    }

    #[test]
    fn a_refused_command_reports_gits_exit_code() {
        let report = render(
            &meta(&["merge", "nope"]),
            &analysis(),
            &Outcome::Failed { code: Some(128) },
            &[],
        );
        assert!(
            report.contains("git refused the command (exit 128)"),
            "{report}"
        );
    }

    #[test]
    fn graphs_are_shown_before_then_after_and_indented_under_their_ref() {
        let graphs = vec![Graph {
            reference: "refs/heads/feature".to_owned(),
            before: "* aaaaaaa old tip".to_owned(),
            after: "* bbbbbbb new tip".to_owned(),
        }];
        let report = render(
            &meta(&["rebase", "main"]),
            &analysis(),
            &Outcome::Clean,
            &graphs,
        );
        assert!(report.contains("graph  refs/heads/feature"), "{report}");
        assert!(
            report.contains("  before\n    * aaaaaaa old tip"),
            "{report}"
        );
        assert!(
            report.contains("  after\n    * bbbbbbb new tip"),
            "{report}"
        );
    }

    #[test]
    fn stat_only_asks_for_no_graphs_and_full_asks_for_them() {
        assert!(Detail::Full.wants_graphs());
        assert!(!Detail::StatOnly.wants_graphs());
        // The default is the whole report: a flag adds terseness, its absence
        // never removes anything.
        assert_eq!(Detail::default(), Detail::Full);
    }

    #[test]
    fn stat_only_spawns_nothing_rather_than_spawning_and_discarding() {
        // The path is not a repository and does not exist, so any `git log`
        // against it fails. Coming back Ok is therefore proof that no process
        // ran at all — which is the point of the flag. A test that only
        // compared the two outputs would pass just as happily on an
        // implementation that drew the graphs and then threw them away.
        let nowhere = Path::new("/git-rehearse/no/such/worktree");

        let skipped = graphs(nowhere, &analysis(), Detail::StatOnly).expect("nothing is spawned");
        assert!(skipped.is_empty());

        graphs(nowhere, &analysis(), Detail::Full)
            .expect_err("the full report really does have to read the repository");
    }

    #[test]
    fn a_report_without_graphs_still_says_everything_that_happened() {
        // What `--stat-only` costs, stated as a test: the graph section, and
        // nothing else. The drift line in particular survives — it is a result
        // of the analysis, which the flag never touches.
        let mut analysis = analysis();
        analysis.drift = vec![drift(Replay {
            changed: vec!["teach the parser about tabs".to_owned()],
            compared: true,
            ..Replay::default()
        })];
        let report = render(&meta(&["rebase", "main"]), &analysis, &Outcome::Clean, &[]);

        assert!(!report.contains("graph  "), "{report}");
        assert!(
            report.contains("refs/heads/feature  aaaaaaaa -> bbbbbbbb"),
            "{report}"
        );
        assert!(report.contains("warning: content drift"), "{report}");
    }

    #[test]
    fn applying_is_only_offered_for_a_clean_rehearsal_that_moved_something() {
        assert!(can_apply(&analysis(), &Outcome::Clean));
        assert!(
            !can_apply(&analysis(), &Outcome::Stopped { conflicts: true }),
            "a sandbox mid-rebase holds no result to transplant"
        );
        assert!(!can_apply(&analysis(), &Outcome::Failed { code: Some(1) }));

        let mut unmoved = analysis();
        unmoved.ref_moves.clear();
        assert!(!can_apply(&unmoved, &Outcome::Clean), "nothing to apply");
    }

    #[test]
    fn without_a_terminal_nothing_is_ever_applied() {
        for outcome in [
            Outcome::Clean,
            Outcome::Stopped { conflicts: true },
            Outcome::Failed { code: Some(1) },
        ] {
            assert_ne!(
                non_interactive(&outcome),
                Choice::Apply,
                "nothing reaches a real repository unasked: {outcome:?}"
            );
        }
    }

    #[test]
    fn without_a_terminal_a_stopped_rehearsal_is_kept_and_the_others_discarded() {
        // The report has just printed a sandbox path and a `continue` command.
        // Discarding here would delete both between printing and reading them.
        assert_eq!(
            non_interactive(&Outcome::Stopped { conflicts: true }),
            Choice::Keep
        );
        assert_eq!(
            non_interactive(&Outcome::Stopped { conflicts: false }),
            Choice::Keep,
            "an interactive `break` stops without conflicts and is just as resumable"
        );
        // Nothing to come back for: reproducible by re-running, or never started.
        assert_eq!(non_interactive(&Outcome::Clean), Choice::Discard);
        assert_eq!(
            non_interactive(&Outcome::Failed { code: Some(1) }),
            Choice::Discard
        );
    }

    #[test]
    fn answers_are_taken_short_or_spelled_out_in_any_case() {
        assert_eq!(parse("a", true), Some(Choice::Apply));
        assert_eq!(parse("Apply", true), Some(Choice::Apply));
        assert_eq!(parse("K", true), Some(Choice::Keep));
        assert_eq!(parse("discard", false), Some(Choice::Discard));
        assert_eq!(parse("", true), None, "Enter must not decide anything");
        assert_eq!(
            parse("a", false),
            None,
            "apply cannot be chosen when it is not offered"
        );
    }

    #[test]
    fn the_prompt_repeats_until_it_gets_an_answer_it_understands() {
        let mut input = Cursor::new(b"\nyes\nk\n".to_vec());
        let mut output = Vec::new();

        let choice = ask(&mut input, &mut output, true).expect("the streams work");

        assert_eq!(choice, Choice::Keep);
        let shown = String::from_utf8(output).expect("utf-8");
        assert_eq!(
            shown.matches("[a]pply / [d]iscard / [k]eep?").count(),
            3,
            "asked once, then again after each answer it could not read: {shown}"
        );
    }

    #[test]
    fn end_of_input_discards_rather_than_looping_forever() {
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();

        let choice = ask(&mut input, &mut output, true).expect("the streams work");

        assert_eq!(choice, Choice::Discard);
    }

    #[test]
    fn ids_are_abbreviated_but_short_ones_survive_intact() {
        assert_eq!(short("aaaaaaaaaaaabbbb"), "aaaaaaaa");
        assert_eq!(short("abc"), "abc");
    }
}
