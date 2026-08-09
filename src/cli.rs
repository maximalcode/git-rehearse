//! The command surface, and the exit codes that become API here.
//!
//! **Hand-rolled argument parsing, deliberately.** The issue asked for the
//! choice to be documented, so: this tool's whole job is to hand somebody
//! else's git command to git untouched. `git rehearse rebase -i --onto x` has
//! to reach git as `rebase -i --onto x`, including flags this program has
//! never heard of and flags that collide with its own. Every argument parser
//! worth using — clap included — is built to *understand* arguments, and
//! bending one into passing them through verbatim costs more than the eighty
//! lines below and hides the one rule that matters:
//!
//! > Our own options come **before** the command. The first word that is not
//! > one of ours starts the git command, and everything after it is git's.
//!
//! So `git rehearse --apply rebase -i main` is unambiguous, and
//! `git rehearse rebase -i main --apply` passes `--apply` to git, where it
//! will be rejected by git rather than silently swallowed by us. The same rule
//! git itself uses for `git -c x=y commit`.
//!
//! It also keeps the dependency tree at two crates, which SCOPE.md asks for.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::execute::{Outcome, SEQUENCE_EDITOR_ARG, Todo};
use crate::report::Choice;
use crate::sandbox::{DEFAULT_TTL_SECS, Sandbox, Status};
use crate::{
    Error, Result, analyze, apply, cache, execute, git, now_unix, preflight, report, sandbox,
};

/// Exit codes. Stable from v0.1 on: v2's agent mode reads these, and SCOPE.md
/// fixes their meanings. Do not spend them on anything else.
pub mod exit {
    /// Rehearsed clean, or a management command succeeded.
    pub const CLEAN: u8 = 0;
    /// Something went wrong inside git-rehearse itself.
    pub const INTERNAL: u8 = 1;
    /// The rehearsed command stopped part-way — usually a conflict.
    pub const STOPPED: u8 = 2;
    /// The rehearsed command failed in the sandbox.
    pub const FAILED: u8 = 3;
    /// We refused: dirty tree, ref race, unsupported repository.
    pub const REFUSED: u8 = 4;
}

/// The usage text. Also the specification of the surface above.
pub const USAGE: &str = "\
git-rehearse — rehearse dangerous git commands in a shadow clone of your repo

usage:
  git rehearse [options] rebase|merge|cherry-pick [git args...]
  git rehearse [options] -- <any git command>
  git rehearse list
  git rehearse show [<id>]
  git rehearse continue [<id>]
  git rehearse apply [<id>]
  git rehearse discard [<id>|--all]

options (before the command; everything after it belongs to git):
  --apply           apply without asking
  --keep            keep the rehearsal without asking
  --todo <file>     drive an interactive rebase from a prepared todo
  -h, --help        this text
  -V, --version     version

exit codes:
  0 rehearsed clean   2 stopped (conflict)   3 command failed
  4 refused           1 internal error

The exit code describes the rehearsal, not what became of it: a rehearsal
that ran cleanly and was then discarded still exits 0.

With no terminal on stdin there is nobody to answer the keep/apply/discard
question, so the rehearsal is discarded. Pass --apply or --keep to script it.
";

/// Said when the question is skipped because there is nobody to answer it.
///
/// Without this the non-interactive path is invisible: exit 0 and an
/// unchanged repository, which is truthful — it rehearsed cleanly — but reads
/// as "applied" to anyone who did not know a question had been skipped and
/// answered on their behalf.
const NOT_A_TERMINAL: &str = "\
stdin is not a terminal, so there was nobody to ask: the rehearsal was discarded.
Use --apply to apply it, or --keep to keep it for `git rehearse apply`.";

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    /// Rehearse a git command.
    Rehearse {
        /// The command, as git will receive it.
        command: Vec<String>,
        /// A prepared rebase todo, from `--todo`.
        todo: Option<PathBuf>,
        /// What to do with the result afterwards.
        decision: Decision,
    },
    /// List kept rehearsals.
    List,
    /// Print a rehearsal's report again.
    Show {
        id: Option<String>,
    },
    /// Carry on a rehearsal that stopped, once its conflicts are resolved.
    Continue {
        id: Option<String>,
        /// What to do with the result — the same decision a rehearsal ends on,
        /// because continuing one ends in exactly the same place.
        decision: Decision,
    },
    /// Apply a rehearsal.
    Apply {
        id: Option<String>,
    },
    /// Throw one — or all — away.
    Discard {
        id: Option<String>,
        all: bool,
    },
    /// git calling us as its sequence editor. Not user-facing.
    SequenceEditor {
        todo: PathBuf,
        target: PathBuf,
    },
}

/// How the question at the end of a rehearsal gets answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Ask, if there is anybody to ask.
    Ask,
    /// `--apply`.
    Apply,
    /// `--keep`.
    Keep,
}

/// Parses the argument list.
///
/// # Errors
///
/// [`Error::Refused`] with a message worth printing, for anything that is not
/// a command this version understands.
pub fn parse(args: &[String]) -> Result<Command> {
    let mut decision = Decision::Ask;
    let mut todo = None;
    let mut rest = args.iter();

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--apply" => decision = Decision::Apply,
            "--keep" => decision = Decision::Keep,
            "--todo" => {
                let file = rest.next().ok_or_else(|| {
                    Error::Refused("--todo needs a file.\nUsage: --todo <file>".to_owned())
                })?;
                todo = Some(PathBuf::from(file));
            }
            // Everything after `--` is git's, whatever it looks like.
            "--" => {
                let command: Vec<String> = rest.cloned().collect();
                return if command.is_empty() {
                    Err(Error::Refused(
                        "`--` needs a git command after it.\n\
                         For example: git rehearse -- rebase -i main"
                            .to_owned(),
                    ))
                } else {
                    Ok(Command::Rehearse {
                        command,
                        todo,
                        decision,
                    })
                };
            }
            SEQUENCE_EDITOR_ARG => {
                let (Some(file), Some(target)) = (rest.next(), rest.next()) else {
                    return Err(Error::Refused(format!(
                        "{SEQUENCE_EDITOR_ARG} is internal and takes exactly two paths"
                    )));
                };
                return Ok(Command::SequenceEditor {
                    todo: PathBuf::from(file),
                    target: PathBuf::from(target),
                });
            }
            "list" => return Ok(Command::List),
            "show" => return Ok(Command::Show { id: id_from(rest) }),
            "continue" => {
                return Ok(Command::Continue {
                    id: id_from(rest),
                    decision,
                });
            }
            "apply" => return Ok(Command::Apply { id: id_from(rest) }),
            "discard" => {
                let arguments: Vec<&String> = rest.collect();
                let all = arguments.iter().any(|arg| *arg == "--all");
                let id = arguments
                    .iter()
                    .find(|arg| !arg.starts_with('-'))
                    .map(|arg| (*arg).clone());
                return Ok(Command::Discard { id, all });
            }
            // The commands worth rehearsing, and the escape hatch for the rest.
            "rebase" | "merge" | "cherry-pick" => {
                let mut command = vec![arg.clone()];
                command.extend(rest.cloned());
                return Ok(Command::Rehearse {
                    command,
                    todo,
                    decision,
                });
            }
            other => {
                return Err(Error::Refused(format!(
                    "'{other}' is not a git-rehearse command.\n\
                     Rehearse rebase, merge or cherry-pick directly, use `--` for anything \
                     else (`git rehearse -- {other}`), or see --help."
                )));
            }
        }
    }
    Ok(Command::Help)
}

/// The first non-flag argument left, if any.
fn id_from<'a>(rest: impl Iterator<Item = &'a String>) -> Option<String> {
    rest.into_iter().find(|arg| !arg.starts_with('-')).cloned()
}

/// Runs a parsed command and returns the process exit code.
///
/// `cwd` is where the user ran us. Everything that touches the terminal goes
/// through `output`, so the whole flow can be driven in a test.
///
/// # Errors
///
/// Whatever the operation returns; the caller turns it into an exit code with
/// [`code_for`].
pub fn run<W: Write>(command: Command, cwd: &Path, output: &mut W) -> Result<u8> {
    match command {
        Command::Help => {
            write!(output, "{USAGE}").map_err(Error::Spawn)?;
            Ok(exit::CLEAN)
        }
        Command::Version => {
            writeln!(output, "git-rehearse {}", env!("CARGO_PKG_VERSION")).map_err(Error::Spawn)?;
            Ok(exit::CLEAN)
        }
        Command::SequenceEditor { todo, target } => {
            execute::write_todo(&todo, &target)?;
            Ok(exit::CLEAN)
        }
        Command::Rehearse {
            command,
            todo,
            decision,
        } => rehearse(&command, todo, decision, cwd, output),
        Command::List => list(cwd, output),
        Command::Show { id } => show(id.as_deref(), cwd, output),
        Command::Continue { id, decision } => resume(id.as_deref(), decision, cwd, output),
        Command::Apply { id } => apply_kept(id.as_deref(), cwd, output),
        Command::Discard { id, all } => discard(id.as_deref(), all, cwd, output),
    }
}

/// The exit code for a result, per SCOPE.md.
#[must_use]
pub fn code_for(result: &Result<u8>) -> u8 {
    match result {
        Ok(code) => *code,
        Err(Error::Refused(_)) => exit::REFUSED,
        Err(_) => exit::INTERNAL,
    }
}

/// The whole rehearsal flow: preflight, sandbox, run, analyse, report, decide.
fn rehearse<W: Write>(
    command: &[String],
    todo: Option<PathBuf>,
    decision: Decision,
    cwd: &Path,
    output: &mut W,
) -> Result<u8> {
    let plan = preflight::run(cwd)?.into_plan(command.to_vec());
    let cache_root = cache::root()?;
    let mut sandbox = sandbox::create(&cache_root, &plan, now_unix())?;

    let todo = todo.map(Todo::new).transpose()?;
    let outcome = execute::run(&sandbox.worktree(), &plan.command, todo.as_ref())?;
    sandbox.record(&outcome)?;

    let code = code_for_outcome(&outcome);
    report_and_decide(sandbox, &outcome, decision, output)?;
    Ok(code)
}

/// Carries on a rehearsal that stopped, then reports on where it got to.
///
/// The resolution itself happens in the sandbox, by whatever means the user
/// likes — an editor, a merge tool, a shell. This is the step that tells git
/// the resolution is ready, which is what `git rebase --continue` means
/// anywhere else. Principle 2 then gives the rest away for free: the
/// resolution is baked into the sandbox's commits, and applying transplants
/// those commits, so what gets applied is what was resolved.
fn resume<W: Write>(
    id: Option<&str>,
    decision: Decision,
    cwd: &Path,
    output: &mut W,
) -> Result<u8> {
    let mut sandbox = find(id, cwd)?;
    let outcome = execute::resume(&sandbox.worktree())?;
    sandbox.record(&outcome)?;

    let code = code_for_outcome(&outcome);
    report_and_decide(sandbox, &outcome, decision, output)?;
    Ok(code)
}

/// Reports on a finished rehearsal, asks what to do with it, and does that.
///
/// Shared by the two routes that arrive here: one has just run a command, the
/// other has just carried one on. From this point they are the same rehearsal
/// with the same decision to make, and the pre-state and command come out of
/// `meta.json` either way — which is also what makes `continue` work in a
/// later process than the rehearsal it continues.
fn report_and_decide<W: Write>(
    sandbox: Sandbox,
    outcome: &Outcome,
    decision: Decision,
    output: &mut W,
) -> Result<()> {
    let worktree = sandbox.worktree();
    let meta = sandbox.meta();
    let analysis = analyze::run(&worktree, &meta.pre_state, &meta.command, outcome)?;
    let graphs = report::graphs(&worktree, &analysis)?;
    // A blank line first: git has just finished writing its own output to the
    // same terminal.
    writeln!(output).map_err(Error::Spawn)?;
    write!(
        output,
        "{}",
        report::render(meta, &analysis, outcome, &graphs)
    )
    .map_err(Error::Spawn)?;

    if matches!(outcome, Outcome::Stopped { .. }) {
        write_next_steps(&sandbox, &worktree, decision, output)?;
    }

    let can_apply = report::can_apply(&analysis, outcome);
    let choice = choose(decision, can_apply, output)?;
    act(choice, sandbox, output)
}

/// Says where the stopped rehearsal is and how to carry it on.
///
/// The report names the unmerged files, which is of no use without the one
/// thing the report cannot know: where they are. Until this was printed, a
/// rehearsal that stopped on a conflict — the case somebody reaches for this
/// tool *for* — ended by describing a problem and pointing nowhere.
fn write_next_steps<W: Write>(
    sandbox: &Sandbox,
    worktree: &Path,
    decision: Decision,
    output: &mut W,
) -> Result<()> {
    writeln!(output).map_err(Error::Spawn)?;
    writeln!(output, "to work on it:").map_err(Error::Spawn)?;
    // A fresh rehearsal is discarded at the end of this run unless it is kept,
    // and the sandbox goes with it — so saying "cd there" without saying that
    // first would be sending the user to a directory about to be deleted.
    //
    // Only when there is still a question to answer, though: with --keep
    // already given, or on a rehearsal that is kept already, telling somebody
    // to answer a prompt they will never see is worse than saying nothing.
    if sandbox.meta().status == Status::Fresh && decision == Decision::Ask {
        writeln!(output, "  answer [k]eep below, then").map_err(Error::Spawn)?;
    }
    writeln!(output, "  cd {}", worktree.display()).map_err(Error::Spawn)?;
    writeln!(output, "  # resolve the conflict, then `git add` the files").map_err(Error::Spawn)?;
    writeln!(output, "  git rehearse continue {}", sandbox.id()).map_err(Error::Spawn)
}

/// The exit code that describes an outcome.
///
/// The code describes the rehearsal, not what was done with it: a conflict is
/// still a conflict whether or not the user kept the sandbox.
fn code_for_outcome(outcome: &Outcome) -> u8 {
    match outcome {
        Outcome::Clean => exit::CLEAN,
        Outcome::Stopped { .. } => exit::STOPPED,
        Outcome::Failed { .. } => exit::FAILED,
    }
}

/// Asks, or decides without asking.
fn choose<W: Write>(decision: Decision, can_apply: bool, output: &mut W) -> Result<Choice> {
    let wanted = match decision {
        Decision::Apply => Some(Choice::Apply),
        Decision::Keep => Some(Choice::Keep),
        Decision::Ask if io::stdin().is_terminal() => None,
        // Nobody to ask: discard unless told otherwise — and say so, because a
        // decision was made on the user's behalf and nothing else would reveal
        // it.
        Decision::Ask => {
            writeln!(output, "\n{NOT_A_TERMINAL}").map_err(Error::Spawn)?;
            Some(report::non_interactive(false, false))
        }
    };
    if let Some(choice) = wanted {
        if choice == Choice::Apply && !can_apply {
            return Err(Error::Refused(
                "--apply was asked for, but this rehearsal has nothing that can be applied.\n\
                 A command that stopped part-way or failed leaves no result to transplant."
                    .to_owned(),
            ));
        }
        return Ok(choice);
    }
    writeln!(output).map_err(Error::Spawn)?;
    let stdin = io::stdin();
    report::ask(&mut stdin.lock(), output, can_apply).map_err(Error::Spawn)
}

/// Carries out the choice.
fn act<W: Write>(choice: Choice, mut sandbox: Sandbox, output: &mut W) -> Result<()> {
    match choice {
        Choice::Apply => {
            let applied = apply::run(&sandbox, now_unix())?;
            report_applied(&applied, output)?;
            sandbox.discard()?;
        }
        Choice::Discard => sandbox.discard()?,
        Choice::Keep => {
            sandbox.keep()?;
            writeln!(
                output,
                "kept as {} — `git rehearse show {}` to see it again",
                sandbox.id(),
                sandbox.id()
            )
            .map_err(Error::Spawn)?;
        }
    }
    Ok(())
}

fn report_applied<W: Write>(applied: &apply::Applied, output: &mut W) -> Result<()> {
    writeln!(output, "applied:").map_err(Error::Spawn)?;
    for moved in &applied.moved {
        // HEAD followed its branch; saying so twice adds nothing.
        if moved.name == preflight::HEAD_KEY {
            continue;
        }
        writeln!(
            output,
            "  {} {}",
            moved.name,
            moved.after.as_deref().unwrap_or("deleted")
        )
        .map_err(Error::Spawn)?;
    }
    if let Some(branch) = &applied.reset {
        writeln!(output, "  worktree reset to {branch}").map_err(Error::Spawn)?;
    }
    writeln!(
        output,
        "where everything was is written down in {}",
        applied.undo.display()
    )
    .map_err(Error::Spawn)?;
    Ok(())
}

/// Lists this repository's rehearsals, pruning expired ones first.
fn list<W: Write>(cwd: &Path, output: &mut W) -> Result<u8> {
    let cache_root = cache::root()?;
    let pruned = sandbox::prune(&cache_root, now_unix(), DEFAULT_TTL_SECS)?;
    let repo_id = repo_id(cwd)?;
    let rehearsals = sandbox::list(&cache_root, Some(&repo_id))?;

    if rehearsals.is_empty() {
        writeln!(output, "no rehearsals for this repository").map_err(Error::Spawn)?;
    }
    for sandbox in &rehearsals {
        let meta = sandbox.meta();
        writeln!(
            output,
            "{}  {:?}  git {}",
            meta.id,
            meta.status,
            meta.command.join(" ")
        )
        .map_err(Error::Spawn)?;
    }
    if !pruned.is_empty() {
        writeln!(
            output,
            "({} rehearsal(s) older than {} days pruned)",
            pruned.len(),
            DEFAULT_TTL_SECS / 86_400
        )
        .map_err(Error::Spawn)?;
    }
    Ok(exit::CLEAN)
}

/// Re-prints a rehearsal's report.
fn show<W: Write>(id: Option<&str>, cwd: &Path, output: &mut W) -> Result<u8> {
    let sandbox = find(id, cwd)?;
    let meta = sandbox.meta();
    // What the command did is remembered rather than re-derived; a rehearsal
    // that was never run is the only case with nothing to remember.
    let outcome = meta.result.clone().ok_or_else(|| {
        Error::Refused(format!(
            "rehearsal {} never ran a command, so there is no report.",
            meta.id
        ))
    })?;
    let analysis = analyze::run(
        &sandbox.worktree(),
        &meta.pre_state,
        &meta.command,
        &outcome,
    )?;
    let graphs = report::graphs(&sandbox.worktree(), &analysis)?;
    write!(
        output,
        "{}",
        report::render(meta, &analysis, &outcome, &graphs)
    )
    .map_err(Error::Spawn)?;
    Ok(exit::CLEAN)
}

/// Applies a rehearsal that was kept.
fn apply_kept<W: Write>(id: Option<&str>, cwd: &Path, output: &mut W) -> Result<u8> {
    let sandbox = find(id, cwd)?;
    let applied = apply::run(&sandbox, now_unix())?;
    report_applied(&applied, output)?;
    sandbox.discard()?;
    Ok(exit::CLEAN)
}

/// Throws rehearsals away.
fn discard<W: Write>(id: Option<&str>, all: bool, cwd: &Path, output: &mut W) -> Result<u8> {
    let cache_root = cache::root()?;
    if all {
        let repo_id = repo_id(cwd)?;
        let mut count = 0;
        for sandbox in sandbox::list(&cache_root, Some(&repo_id))? {
            sandbox.discard()?;
            count += 1;
        }
        writeln!(output, "discarded {count} rehearsal(s)").map_err(Error::Spawn)?;
        return Ok(exit::CLEAN);
    }
    let sandbox = find(id, cwd)?;
    let discarded = sandbox.id().to_owned();
    sandbox.discard()?;
    writeln!(output, "discarded {discarded}").map_err(Error::Spawn)?;
    Ok(exit::CLEAN)
}

fn find(id: Option<&str>, cwd: &Path) -> Result<Sandbox> {
    sandbox::find(&cache::root()?, &repo_id(cwd)?, id)
}

/// The cache directory name for the repository `cwd` is in.
///
/// Deliberately not a full preflight: `list` and `discard` have to work in a
/// repository preflight would refuse to rehearse — that is often exactly when
/// somebody wants to clean up.
fn repo_id(cwd: &Path) -> Result<String> {
    let top = git::run(cwd, ["rev-parse", "--show-toplevel"]).map_err(|_| {
        Error::Refused(format!(
            "not a git repository: {}\n\
             Run git-rehearse from inside the repository you rehearsed against.",
            cwd.display()
        ))
    })?;
    Ok(cache::repo_id(&git::canonicalize(&PathBuf::from(top))?))
}

#[cfg(test)]
mod tests {
    use super::{Command, Decision, code_for, exit, parse};
    use crate::Error;
    use std::path::PathBuf;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn command(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn a_rehearsal_passes_every_git_argument_through_untouched() {
        assert_eq!(
            parse(&args(&["rebase", "-i", "--onto", "main", "HEAD~3"])).expect("parses"),
            Command::Rehearse {
                command: command(&["rebase", "-i", "--onto", "main", "HEAD~3"]),
                todo: None,
                decision: Decision::Ask,
            }
        );
    }

    #[test]
    fn our_own_flags_after_the_command_belong_to_git() {
        // git will reject it, loudly and in git's own words, which is better
        // than us swallowing an argument the user meant for git.
        let Command::Rehearse {
            command, decision, ..
        } = parse(&args(&["rebase", "main", "--apply"])).expect("parses")
        else {
            panic!("expected a rehearsal");
        };
        assert_eq!(command, self::command(&["rebase", "main", "--apply"]));
        assert_eq!(decision, Decision::Ask, "--apply here was not ours");
    }

    #[test]
    fn our_own_flags_before_the_command_are_ours() {
        let Command::Rehearse {
            command,
            decision,
            todo,
        } = parse(&args(&[
            "--apply",
            "--todo",
            "/tmp/todo",
            "rebase",
            "-i",
            "main",
        ]))
        .expect("parses")
        else {
            panic!("expected a rehearsal");
        };
        assert_eq!(command, self::command(&["rebase", "-i", "main"]));
        assert_eq!(decision, Decision::Apply);
        assert_eq!(todo, Some(PathBuf::from("/tmp/todo")));
    }

    #[test]
    fn the_escape_hatch_takes_anything() {
        assert_eq!(
            parse(&args(&["--", "filter-branch", "--tree-filter", "rm -f x"])).expect("parses"),
            Command::Rehearse {
                command: command(&["filter-branch", "--tree-filter", "rm -f x"]),
                todo: None,
                decision: Decision::Ask,
            }
        );
    }

    #[test]
    fn an_unknown_command_points_at_the_escape_hatch() {
        let error = parse(&args(&["status"])).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("git rehearse -- status"), "{message}");
    }

    #[test]
    fn management_commands_take_an_optional_id() {
        assert_eq!(parse(&args(&["list"])).expect("parses"), Command::List);
        assert_eq!(
            parse(&args(&["show"])).expect("parses"),
            Command::Show { id: None }
        );
        assert_eq!(
            parse(&args(&["apply", "1786248000-00"])).expect("parses"),
            Command::Apply {
                id: Some("1786248000-00".to_owned())
            }
        );
        assert_eq!(
            parse(&args(&["discard", "--all"])).expect("parses"),
            Command::Discard {
                id: None,
                all: true
            }
        );
        assert_eq!(
            parse(&args(&["discard", "1786"])).expect("parses"),
            Command::Discard {
                id: Some("1786".to_owned()),
                all: false
            }
        );
    }

    #[test]
    fn continue_takes_an_optional_id_and_honours_the_decision_flags() {
        assert_eq!(
            parse(&args(&["continue"])).expect("parses"),
            Command::Continue {
                id: None,
                decision: Decision::Ask
            }
        );
        // The flags have to reach `continue`: carrying a rehearsal on ends in
        // exactly the same apply/keep/discard question a rehearsal ends on, so
        // a scripted continue that ignored --keep would silently discard the
        // work it had just advanced.
        assert_eq!(
            parse(&args(&["--keep", "continue", "1786"])).expect("parses"),
            Command::Continue {
                id: Some("1786".to_owned()),
                decision: Decision::Keep
            }
        );
        assert_eq!(
            parse(&args(&["--apply", "continue"])).expect("parses"),
            Command::Continue {
                id: None,
                decision: Decision::Apply
            }
        );
    }

    #[test]
    fn help_and_version_win_wherever_they_appear() {
        assert_eq!(parse(&args(&[])).expect("parses"), Command::Help);
        assert_eq!(parse(&args(&["--help"])).expect("parses"), Command::Help);
        assert_eq!(
            parse(&args(&["--apply", "--version"])).expect("parses"),
            Command::Version
        );
    }

    #[test]
    fn a_todo_without_a_file_is_refused_rather_than_ignored() {
        let error = parse(&args(&["--todo"])).expect_err("refused");
        assert!(error.to_string().contains("--todo needs a file"), "{error}");
    }

    #[test]
    fn an_empty_escape_hatch_is_refused() {
        let error = parse(&args(&["--"])).expect_err("refused");
        assert!(error.to_string().contains("needs a git command"), "{error}");
    }

    #[test]
    fn git_can_still_invoke_us_as_its_sequence_editor() {
        assert_eq!(
            parse(&args(&[
                "__sequence-editor",
                "/tmp/todo",
                "/repo/.git/todo"
            ]))
            .expect("parses"),
            Command::SequenceEditor {
                todo: PathBuf::from("/tmp/todo"),
                target: PathBuf::from("/repo/.git/todo"),
            }
        );
    }

    #[test]
    fn refusals_and_internal_errors_get_different_exit_codes() {
        // v2's agent mode reads these; they are API from here on.
        assert_eq!(code_for(&Ok(exit::STOPPED)), 2);
        assert_eq!(
            code_for(&Err(Error::Refused("dirty".to_owned()))),
            exit::REFUSED
        );
        assert_eq!(
            code_for(&Err(Error::NoCacheDir)),
            exit::INTERNAL,
            "a bug of ours is not a refusal"
        );
    }
}
