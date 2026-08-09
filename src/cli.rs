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
    Error, Result, analyze, apply, cache, execute, git, json, now_unix, preflight, report, sandbox,
};

/// Whether the run talks to a person or to a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// The terminal report.
    #[default]
    Text,
    /// One JSON document on stdout and nothing else — see [`crate::json`].
    Json,
}

/// A parsed command line: what to do, and who the output is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub command: Command,
    pub format: Format,
}

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
  --json            one JSON document on stdout instead of the report
  --todo <file>     drive an interactive rebase from a prepared todo
  -h, --help        this text
  -V, --version     version

exit codes:
  0 rehearsed clean   2 stopped (conflict)   3 command failed
  4 refused           1 internal error

The exit code describes the rehearsal, not what became of it: a rehearsal
that ran cleanly and was then discarded still exits 0.

With no terminal on stdin there is nobody to answer the keep/apply/discard
question. A rehearsal that ran cleanly is then discarded; one that stopped
part-way is kept, because its sandbox is the only copy of where it got to and
`continue` needs it. Pass --apply or --keep to decide up front either way.

--json prints one document and nothing else, on every exit path including
failures. It never prompts, for the same reason: there is nobody there.
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

/// Said instead when the rehearsal stopped part-way.
///
/// A stopped rehearsal is kept rather than discarded (see
/// [`report::non_interactive`]), so this has to say something different — and
/// it has to say it here, because the alternative is a run that prints
/// directions to a sandbox and then deletes it.
const NOT_A_TERMINAL_STOPPED: &str = "\
stdin is not a terminal, so there was nobody to ask: the rehearsal was kept, because
it stopped part-way and its sandbox is the only copy of where it got to.
Carry it on with `git rehearse continue`, or throw it away with `git rehearse discard`.";

/// Said when `--apply` asks for something that does not exist.
const NOTHING_TO_APPLY: &str = "\
--apply was asked for, but this rehearsal has nothing that can be applied.
A command that stopped part-way or failed leaves no result to transplant.";

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
pub fn parse(args: &[String]) -> Result<Parsed> {
    let mut format = Format::Text;
    let mut decision = Decision::Ask;
    let mut todo = None;
    let mut rest = args.iter();

    // Every `return` below goes through this, so the format reaches the caller
    // whichever branch the command falls into.
    macro_rules! parsed {
        ($command:expr) => {
            return Ok(Parsed {
                command: $command,
                format,
            })
        };
    }

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => parsed!(Command::Help),
            "-V" | "--version" => parsed!(Command::Version),
            "--json" => format = Format::Json,
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
                if command.is_empty() {
                    return Err(Error::Refused(
                        "`--` needs a git command after it.\n\
                         For example: git rehearse -- rebase -i main"
                            .to_owned(),
                    ));
                }
                parsed!(Command::Rehearse {
                    command,
                    todo,
                    decision,
                });
            }
            SEQUENCE_EDITOR_ARG => {
                let (Some(file), Some(target)) = (rest.next(), rest.next()) else {
                    return Err(Error::Refused(format!(
                        "{SEQUENCE_EDITOR_ARG} is internal and takes exactly two paths"
                    )));
                };
                parsed!(Command::SequenceEditor {
                    todo: PathBuf::from(file),
                    target: PathBuf::from(target),
                });
            }
            "list" => parsed!(Command::List),
            "show" => parsed!(Command::Show { id: id_from(rest) }),
            "continue" => {
                parsed!(Command::Continue {
                    id: id_from(rest),
                    decision,
                });
            }
            "apply" => parsed!(Command::Apply { id: id_from(rest) }),
            "discard" => {
                let arguments: Vec<&String> = rest.collect();
                let all = arguments.iter().any(|arg| *arg == "--all");
                let id = arguments
                    .iter()
                    .find(|arg| !arg.starts_with('-'))
                    .map(|arg| (*arg).clone());
                parsed!(Command::Discard { id, all });
            }
            // The commands worth rehearsing, and the escape hatch for the rest.
            "rebase" | "merge" | "cherry-pick" => {
                let mut command = vec![arg.clone()];
                command.extend(rest.cloned());
                parsed!(Command::Rehearse {
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
    Ok(Parsed {
        command: Command::Help,
        format,
    })
}

/// Whether `--json` was asked for, read straight off the argument list.
///
/// Only for the path where [`parse`] itself failed: a refusal has to be
/// reported in the format the caller asked for, and by then there is no
/// [`Parsed`] to read it from.
///
/// It stops at the first word that is not one of our options, which is the same
/// rule [`parse`] applies — so `git rehearse -- log --json` asks git for
/// `--json` and not us, exactly as it reads.
#[must_use]
pub fn wants_json(args: &[String]) -> bool {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => return true,
            "--apply" | "--keep" => {}
            // Its value too: a path does not start with `-`, and letting it end
            // the scan would hide a `--json` that came after it.
            "--todo" => {
                rest.next();
            }
            // Anything else is the command, or an error inside it. Either way
            // this scan is finished — a `--json` beyond here is git's.
            _ => return false,
        }
    }
    false
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
pub fn run<W: Write>(parsed: Parsed, cwd: &Path, output: &mut W) -> Result<u8> {
    let Parsed { command, format } = parsed;
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
        } => rehearse(&command, todo, decision, format, cwd, output),
        Command::List => list(format, cwd, output),
        Command::Show { id } => show(id.as_deref(), format, cwd, output),
        Command::Continue { id, decision } => resume(id.as_deref(), decision, format, cwd, output),
        Command::Apply { id } => apply_kept(id.as_deref(), format, cwd, output),
        Command::Discard { id, all } => discard(id.as_deref(), all, format, cwd, output),
    }
}

/// Prints a document and nothing else.
///
/// One line, newline-terminated: a caller reading a stream gets a complete
/// document per line, which is exactly what MCP's stdio framing wants if the
/// server in SCOPE's v2 ever gets built.
/// Where git's own stdout belongs, given who is reading ours.
///
/// Git writes `Auto-merging …` and `CONFLICT …` to stdout. Under `--json` that
/// lands in front of the document and breaks any caller that parses the stream,
/// so it is moved to stderr — where it is still readable, just not in the way.
fn chatter(format: Format) -> git::Chatter {
    match format {
        Format::Text => git::Chatter::Inherit,
        Format::Json => git::Chatter::ToStderr,
    }
}

/// Prints the failure document for a run that did not get as far as a report.
///
/// # Errors
///
/// [`Error::Spawn`] if stdout cannot be written.
pub fn write_failure<W: Write>(message: &str, exit_code: u8, output: &mut W) -> Result<()> {
    write_json(&json::Failure::new(message.to_owned(), exit_code), output)
}

fn write_json<W: Write, T: serde::Serialize>(document: &T, output: &mut W) -> Result<()> {
    let text = serde_json::to_string(document)
        .map_err(|e| Error::Sandbox(format!("could not build the JSON report: {e}")))?;
    writeln!(output, "{text}").map_err(Error::Spawn)
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
    format: Format,
    cwd: &Path,
    output: &mut W,
) -> Result<u8> {
    let plan = preflight::run(cwd)?.into_plan(command.to_vec());
    let cache_root = cache::root()?;
    let mut sandbox = sandbox::create(&cache_root, &plan, now_unix())?;

    let todo = todo.map(Todo::new).transpose()?;
    let outcome = execute::run_with(
        &sandbox.worktree(),
        &plan.command,
        todo.as_ref(),
        chatter(format),
    )?;
    sandbox.record(&outcome)?;

    let code = code_for_outcome(&outcome);
    report_and_decide(sandbox, &outcome, decision, format, code, output)?;
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
    format: Format,
    cwd: &Path,
    output: &mut W,
) -> Result<u8> {
    let mut sandbox = find(id, cwd)?;
    let outcome = execute::resume_with(&sandbox.worktree(), chatter(format))?;
    sandbox.record(&outcome)?;

    let code = code_for_outcome(&outcome);
    report_and_decide(sandbox, &outcome, decision, format, code, output)?;
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
    format: Format,
    exit_code: u8,
    output: &mut W,
) -> Result<()> {
    let worktree = sandbox.worktree();
    let meta = sandbox.meta();
    let analysis = analyze::run(&worktree, &meta.pre_state, &meta.command, outcome)?;
    let can_apply = report::can_apply(&analysis, outcome);

    if format == Format::Json {
        return decide_as_json(
            sandbox, &analysis, outcome, decision, exit_code, can_apply, output,
        );
    }

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

    // Whether a question is actually going to be put — not merely whether one
    // was wanted. `Decision::Ask` means "ask if there is anybody to ask", and
    // the next steps below tell the reader to answer a prompt, so they have to
    // be gated on the prompt existing rather than on it having been requested.
    let will_prompt = decision == Decision::Ask && io::stdin().is_terminal();

    if matches!(outcome, Outcome::Stopped { .. }) {
        write_next_steps(&sandbox, &worktree, will_prompt, output)?;
    }

    let choice = choose(decision, will_prompt, can_apply, outcome, output)?;
    if choice == Choice::Apply && !can_apply {
        return Err(refuse_apply(sandbox, outcome));
    }
    act(choice, sandbox, output)
}

/// Refuses an `--apply` that cannot be honoured, without abandoning the sandbox.
///
/// Returning the refusal on its own leaves the rehearsal in the cache with
/// nobody having asked for it — never `Kept`, so `list` shows a `Fresh` entry
/// that no code path deliberately creates, and it sits there for the full TTL
/// (#51).
///
/// What becomes of it follows the same rule as an unanswered question: a
/// stopped rehearsal keeps its sandbox, because that sandbox is the only copy
/// of where the command got to, and anything else lets it go. One exception —
/// a rehearsal that was already `Kept` was kept on purpose by an earlier run,
/// and a refusal here is no reason to undo that.
///
/// The refusal then says which happened, because "cannot be applied" without
/// "and here is where it went" is half an answer.
fn refuse_apply(mut sandbox: Sandbox, outcome: &Outcome) -> Error {
    let id = sandbox.id().to_owned();
    let already_kept = sandbox.meta().status == Status::Kept;

    if already_kept || report::non_interactive(outcome) == Choice::Keep {
        if !already_kept && let Err(error) = sandbox.keep() {
            return error;
        }
        return Error::Refused(format!(
            "{NOTHING_TO_APPLY}\n\
             The rehearsal is kept as {id} — `git rehearse continue {id}` to carry it on, \
             or `git rehearse discard {id}` to throw it away."
        ));
    }

    if let Err(error) = sandbox.discard() {
        return error;
    }
    Error::Refused(NOTHING_TO_APPLY.to_owned())
}

/// The same decision, made without a prompt, reported as one document.
///
/// `--json` and the prompt cannot coexist: the question would land in the
/// middle of the document, and the caller reading it is a program with no way
/// to answer. So the unattended rule decides — which only became a sane default
/// with #48, since before it a stopped rehearsal was discarded and the `id`
/// handed back would already have been dead on arrival.
///
/// The document is built *after* applying or keeping and *before* discarding,
/// because it reports both what was done and where the sandbox is.
fn decide_as_json<W: Write>(
    mut sandbox: Sandbox,
    analysis: &analyze::Analysis,
    outcome: &Outcome,
    decision: Decision,
    exit_code: u8,
    can_apply: bool,
    output: &mut W,
) -> Result<()> {
    let choice = match decision {
        Decision::Apply => Choice::Apply,
        Decision::Keep => Choice::Keep,
        Decision::Ask => report::non_interactive(outcome),
    };
    if choice == Choice::Apply && !can_apply {
        return Err(refuse_apply(sandbox, outcome));
    }

    let applied = if choice == Choice::Apply {
        Some(apply::run(&sandbox, now_unix())?)
    } else {
        None
    };
    if choice == Choice::Keep {
        sandbox.keep()?;
    }

    let document = json::Report::new(
        &sandbox,
        analysis,
        outcome,
        exit_code,
        can_apply,
        choice,
        applied.as_ref(),
    );
    write_json(&document, output)?;

    // Applying transplants the refs and then the sandbox has served its
    // purpose, exactly as it does on the text path.
    if choice != Choice::Keep {
        sandbox.discard()?;
    }
    Ok(())
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
    will_prompt: bool,
    output: &mut W,
) -> Result<()> {
    writeln!(output).map_err(Error::Spawn)?;
    writeln!(output, "to work on it:").map_err(Error::Spawn)?;
    // A fresh rehearsal is discarded at the end of this run unless it is kept,
    // and the sandbox goes with it — so saying "cd there" without saying that
    // first would be sending the user to a directory about to be deleted.
    //
    // Only when a prompt is actually coming, though. With --keep already given,
    // or on a rehearsal that is kept already, telling somebody to answer a
    // question they will never see is worse than saying nothing — and that is
    // just as true when the question is skipped for want of a terminal, which
    // is the case an agent is always in.
    if sandbox.meta().status == Status::Fresh && will_prompt {
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
///
/// Whether the choice is a *possible* one is not settled here — a `--apply`
/// that cannot be honoured is refused by the caller, which owns the sandbox and
/// therefore can dispose of it on the way out. See [`refuse_apply`].
fn choose<W: Write>(
    decision: Decision,
    will_prompt: bool,
    can_apply: bool,
    outcome: &Outcome,
    output: &mut W,
) -> Result<Choice> {
    let wanted = match decision {
        Decision::Apply => Some(Choice::Apply),
        Decision::Keep => Some(Choice::Keep),
        Decision::Ask if will_prompt => None,
        // Nobody to ask, so a decision gets made on the user's behalf — and it
        // has to be announced, because nothing else in the output would reveal
        // that a question was skipped at all.
        Decision::Ask => {
            let choice = report::non_interactive(outcome);
            let notice = if choice == Choice::Keep {
                NOT_A_TERMINAL_STOPPED
            } else {
                NOT_A_TERMINAL
            };
            writeln!(output, "\n{notice}").map_err(Error::Spawn)?;
            Some(choice)
        }
    };
    if let Some(choice) = wanted {
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
fn list<W: Write>(format: Format, cwd: &Path, output: &mut W) -> Result<u8> {
    let cache_root = cache::root()?;
    let pruned = sandbox::prune(&cache_root, now_unix(), DEFAULT_TTL_SECS)?;
    let repo_id = repo_id(cwd)?;
    let rehearsals = sandbox::list(&cache_root, Some(&repo_id))?;

    if format == Format::Json {
        let document = json::Listing {
            schema: json::SCHEMA,
            rehearsals: rehearsals.iter().map(json::Entry::of).collect(),
            pruned,
        };
        write_json(&document, output)?;
        return Ok(exit::CLEAN);
    }

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
fn show<W: Write>(id: Option<&str>, format: Format, cwd: &Path, output: &mut W) -> Result<u8> {
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

    if format == Format::Json {
        // `show` decides nothing — the rehearsal is sitting in the cache, which
        // is what `kept` means. The exit code is this command's, not the
        // rehearsal's: looking at a stopped rehearsal succeeded.
        let document = json::Report::new(
            &sandbox,
            &analysis,
            &outcome,
            exit::CLEAN,
            report::can_apply(&analysis, &outcome),
            Choice::Keep,
            None,
        );
        write_json(&document, output)?;
        return Ok(exit::CLEAN);
    }

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
fn apply_kept<W: Write>(
    id: Option<&str>,
    format: Format,
    cwd: &Path,
    output: &mut W,
) -> Result<u8> {
    let sandbox = find(id, cwd)?;
    let applied = apply::run(&sandbox, now_unix())?;
    if format == Format::Json {
        let document = json::ApplyResult {
            schema: json::SCHEMA,
            id: sandbox.id().to_owned(),
            repository: sandbox.meta().repo_path.display().to_string(),
            exit_code: exit::CLEAN,
            applied: json::applied_report(&applied),
        };
        write_json(&document, output)?;
    } else {
        report_applied(&applied, output)?;
    }
    sandbox.discard()?;
    Ok(exit::CLEAN)
}

/// Throws rehearsals away.
fn discard<W: Write>(
    id: Option<&str>,
    all: bool,
    format: Format,
    cwd: &Path,
    output: &mut W,
) -> Result<u8> {
    let cache_root = cache::root()?;
    let mut discarded = Vec::new();
    if all {
        let repo_id = repo_id(cwd)?;
        for sandbox in sandbox::list(&cache_root, Some(&repo_id))? {
            discarded.push(sandbox.id().to_owned());
            sandbox.discard()?;
        }
    } else {
        let sandbox = find(id, cwd)?;
        discarded.push(sandbox.id().to_owned());
        sandbox.discard()?;
    }

    if format == Format::Json {
        let document = json::DiscardResult {
            schema: json::SCHEMA,
            discarded,
            exit_code: exit::CLEAN,
        };
        write_json(&document, output)?;
    } else if all {
        writeln!(output, "discarded {} rehearsal(s)", discarded.len()).map_err(Error::Spawn)?;
    } else {
        writeln!(output, "discarded {}", discarded.join(", ")).map_err(Error::Spawn)?;
    }
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
    use super::{Command, Decision, Format, code_for, exit, wants_json};
    use crate::{Error, Result};
    use std::path::PathBuf;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// The command half of a parse.
    ///
    /// Shadows [`super::parse`] so the assertions below stay about the command
    /// surface, which is what they are for. The format has its own tests.
    fn parse(args: &[String]) -> Result<Command> {
        super::parse(args).map(|parsed| parsed.command)
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

    /// The format half of a parse.
    fn format(argv: &[&str]) -> Format {
        super::parse(&args(argv)).expect("parses").format
    }

    #[test]
    fn json_is_one_of_our_options_and_obeys_the_same_rule_as_the_rest() {
        assert_eq!(format(&["--json", "rebase", "main"]), Format::Json);
        assert_eq!(format(&["rebase", "main"]), Format::Text);
        // Every command, not just a rehearsal: the flag is parsed before the
        // command, so `--json list` gets typed by anyone who has it in mind,
        // and printing human text there would be the worst of both.
        assert_eq!(format(&["--json", "list"]), Format::Json);
        assert_eq!(format(&["--json", "show"]), Format::Json);
        assert_eq!(format(&["--json", "apply"]), Format::Json);
        assert_eq!(format(&["--json", "discard", "--all"]), Format::Json);
        assert_eq!(format(&["--json", "continue"]), Format::Json);
    }

    #[test]
    fn json_after_the_command_belongs_to_git_like_every_other_flag() {
        // Same rule as --apply: ours come first. git will reject it in git's
        // own words rather than us swallowing an argument meant for git.
        let Command::Rehearse { command, .. } =
            parse(&args(&["rebase", "main", "--json"])).expect("parses")
        else {
            panic!("expected a rehearsal");
        };
        assert_eq!(command, self::command(&["rebase", "main", "--json"]));
        assert_eq!(format(&["rebase", "main", "--json"]), Format::Text);
        assert_eq!(format(&["--", "log", "--json"]), Format::Text);
    }

    #[test]
    fn the_failure_path_reads_the_flag_off_the_arguments() {
        // main() needs the format for a run where parse() itself failed, so
        // wants_json has to agree with parse on where our options stop.
        assert!(wants_json(&args(&["--json", "rebase", "main"])));
        assert!(wants_json(&args(&["--keep", "--json", "status"])));
        assert!(
            wants_json(&args(&["--todo", "/tmp/todo", "--json", "rebase"])),
            "--todo's value must not end the scan"
        );
        assert!(!wants_json(&args(&["rebase", "main", "--json"])));
        assert!(!wants_json(&args(&["--", "log", "--json"])));
        assert!(!wants_json(&args(&["rebase", "main"])));
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
