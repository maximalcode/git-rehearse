//! Running the rehearsed command — the moment design principle 1 is either
//! kept or broken.
//!
//! The user's `git` binary runs the user's command, in the sandbox, with the
//! user's environment and terminal. Nothing is captured, parsed, filtered or
//! reimplemented: an interactive rebase opens the real editor, the progress
//! output is the real progress output, and merge drivers, `rerere` and
//! attributes behave exactly as they would have in the real repository —
//! because they *are* the same configuration.
//!
//! What this module adds is only what happens either side of that: an
//! optional injected rebase todo, and turning git's exit status into the three
//! outcomes the exit codes are built on.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use serde::{Deserialize, Serialize};

use crate::{Error, Result, git};

/// The argument that makes this binary act as git's sequence editor.
///
/// Double-underscored because it is not part of the command surface: git runs
/// it, users do not.
pub const SEQUENCE_EDITOR_ARG: &str = "__sequence-editor";

/// How the rehearsed command ended.
///
/// These three map onto the exit codes SCOPE.md fixes from v0.1 (`0` clean,
/// `2` stopped, `3` failed), which v2's agent mode depends on — so the
/// distinction between "stopped, and you can look at it" and "git refused" is
/// load-bearing, not cosmetic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Outcome {
    /// The command finished and left no operation in progress.
    Clean,
    /// The command stopped part-way and git is waiting: a conflict, or an
    /// `edit`/`break` in an interactive todo. The sandbox is worth looking at.
    Stopped {
        /// Whether the stop involves unmerged paths — a `break` stops without
        /// any, and a report that called that a conflict would be lying.
        conflicts: bool,
    },
    /// Git refused the command outright and left nothing in progress.
    Failed {
        /// Git's exit code, or `None` if a signal killed it.
        code: Option<i32>,
    },
}

/// A rebase todo to inject instead of the one git would generate.
///
/// Cheap to support now and load-bearing later: an agent that can hand over a
/// todo can rehearse a whole planned history rewrite without a terminal, which
/// is what v2 is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    /// The todo to install, in git's own `pick <sha> <subject>` format.
    pub file: PathBuf,
    /// The binary git should run as its sequence editor — this one.
    ///
    /// A parameter rather than a call to [`std::env::current_exe`] inside,
    /// because under `cargo test` that would be the test harness. The default
    /// from [`Todo::new`] is the right thing everywhere else.
    pub editor: PathBuf,
}

impl Todo {
    /// A todo installed by the currently running binary.
    ///
    /// # Errors
    ///
    /// [`Error::Sandbox`] if the running executable's path cannot be
    /// determined, which is the only way this can fail.
    pub fn new(file: PathBuf) -> Result<Self> {
        let editor = std::env::current_exe().map_err(|err| {
            Error::Sandbox(format!("cannot locate the git-rehearse binary: {err}"))
        })?;
        Ok(Self { file, editor })
    }
}

/// Runs `command` in the sandbox worktree.
///
/// # Errors
///
/// [`Error::Refused`] if an injected todo cannot apply to this command,
/// [`Error::Spawn`] if git cannot be started, [`Error::Git`] if inspecting the
/// stopped state afterwards fails.
pub fn run(worktree: &Path, command: &[String], todo: Option<&Todo>) -> Result<Outcome> {
    let env = match todo {
        Some(todo) => vec![("GIT_SEQUENCE_EDITOR", sequence_editor(todo, command)?)],
        None => Vec::new(),
    };
    let status = git::spawn(worktree, command, &env)?;
    classify(worktree, status)
}

/// The `GIT_SEQUENCE_EDITOR` value that installs `todo`, after checking that
/// git will actually consult it.
fn sequence_editor(todo: &Todo, command: &[String]) -> Result<OsString> {
    if !todo.file.is_file() {
        return Err(Error::Refused(format!(
            "no such todo file: {}\n\
             --todo takes a file containing rebase instructions (`pick <sha>` lines).",
            todo.file.display()
        )));
    }
    // Refusing beats guessing: git only consults the sequence editor for an
    // interactive rebase, so quietly adding -i would mean running a command
    // the user did not write, and quietly dropping the todo would mean
    // reporting on a rebase that ignored it.
    if command.first().map(String::as_str) != Some("rebase") {
        return Err(Error::Refused(
            "--todo only applies to `rebase`.\n\
             Rehearse a rebase, or drop --todo."
                .to_owned(),
        ));
    }
    if !command
        .iter()
        .any(|arg| arg == "-i" || arg == "--interactive")
    {
        return Err(Error::Refused(
            "--todo needs an interactive rebase.\n\
             Add -i to the rebase you are rehearsing."
                .to_owned(),
        ));
    }

    // git hands this string to the shell and appends the todo path it wants
    // written, so both halves are quoted.
    Ok(OsString::from(format!(
        "{} {SEQUENCE_EDITOR_ARG} {}",
        shell_quote(&todo.editor),
        shell_quote(&todo.file)
    )))
}

/// Installs a prepared todo over the one git generated.
///
/// This is what runs when git invokes us as its sequence editor: `target` is
/// the `git-rebase-todo` path git passes as the last argument.
///
/// # Errors
///
/// [`Error::Io`] if either file cannot be read or written.
pub fn write_todo(todo: &Path, target: &Path) -> Result<()> {
    // Contents rather than `fs::copy`, which would carry the source file's
    // permissions onto a file inside git's sequencer directory.
    let contents = fs::read(todo).map_err(Error::io(todo))?;
    fs::write(target, contents).map_err(Error::io(target))
}

/// An operation git has left half-finished in a sandbox.
///
/// Named separately from the rehearsed command because the two are not the
/// same question. `git rehearse rebase main` and `git rehearse -- rebase main`
/// leave identical state, a `--` rehearsal may have no first word worth
/// trusting, and a rebase that stopped is a rebase regardless of how it was
/// spelled. So this is read off the sequencer's own files — the same ones
/// `git status` reads — rather than inferred from what the user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Rebase,
    CherryPick,
    Revert,
    Merge,
    /// A bisect in progress. Listed because it *is* an operation in progress,
    /// and left un-resumable on purpose: `git bisect` has no `--continue`, and
    /// inventing one out of `git bisect good|bad` would be guessing at an
    /// answer only the user has.
    Bisect,
}

impl Operation {
    /// The git subcommand that carries this operation on.
    fn subcommand(self) -> Option<&'static str> {
        match self {
            Self::Rebase => Some("rebase"),
            Self::CherryPick => Some("cherry-pick"),
            Self::Revert => Some("revert"),
            Self::Merge => Some("merge"),
            Self::Bisect => None,
        }
    }
}

/// What git left half-finished in the sandbox, if anything.
///
/// Rebase is checked first because it is the one that overlaps: while a rebase
/// replays commits it may also leave `CHERRY_PICK_HEAD` behind, and answering
/// "cherry-pick" for a stopped rebase would send `git cherry-pick --continue`
/// at a sequencer that wanted `git rebase --continue`.
///
/// # Errors
///
/// [`Error::Git`] if the sandbox's git directory cannot be located.
pub fn in_progress(worktree: &Path) -> Result<Option<Operation>> {
    let git_dir = PathBuf::from(git::run(worktree, ["rev-parse", "--absolute-git-dir"])?);
    let exists = |marker: &str| git_dir.join(marker).exists();

    Ok(if exists("rebase-merge") || exists("rebase-apply") {
        Some(Operation::Rebase)
    } else if exists("CHERRY_PICK_HEAD") {
        Some(Operation::CherryPick)
    } else if exists("REVERT_HEAD") {
        Some(Operation::Revert)
    } else if exists("MERGE_HEAD") {
        Some(Operation::Merge)
    } else if exists("BISECT_LOG") {
        Some(Operation::Bisect)
    } else {
        None
    })
}

/// Every path git still considers unmerged in the sandbox.
///
/// # Errors
///
/// [`Error::Git`] if the index cannot be read.
pub fn unmerged(worktree: &Path) -> Result<Vec<String>> {
    let listing = git::run(worktree, ["diff", "--name-only", "--diff-filter=U", "-z"])?;
    Ok(listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Carries on the operation the sandbox stopped in the middle of.
///
/// Runs `git <operation> --continue` through the same terminal and editor as
/// the original command, for the same reason [`run`] does: the commit message
/// git proposes for a resolved conflict is the user's to approve, and
/// capturing or suppressing that would be this crate deciding something it has
/// no business deciding.
///
/// Nothing here resolves anything. The resolution happens in the sandbox, by
/// whatever means the user likes, and this is the step that tells git the
/// resolution is ready — which is exactly what `git rebase --continue` means
/// in a repository that is not a sandbox.
///
/// # Errors
///
/// [`Error::Refused`] if there is nothing to continue, if paths are still
/// unmerged, or for a bisect, which cannot be continued mechanically.
/// [`Error::Spawn`] if git cannot be started.
pub fn resume(worktree: &Path) -> Result<Outcome> {
    let Some(operation) = in_progress(worktree)? else {
        return Err(Error::Refused(
            "this rehearsal has nothing in progress — there is nothing to continue.\n\
             `git rehearse show` prints the report again; `apply` transplants it."
                .to_owned(),
        ));
    };
    let Some(subcommand) = operation.subcommand() else {
        return Err(Error::Refused(
            "this rehearsal stopped in a bisect, which cannot be continued for you.\n\
             A bisect advances on your answer — mark the commit yourself inside the \
             sandbox with `git bisect good|bad`."
                .to_owned(),
        ));
    };

    // Refused before git is even started: `--continue` on an unresolved
    // conflict fails with git's own message about staging, which is correct
    // but arrives after the user has been told the rehearsal is resuming.
    // Naming the files that still need attention is the more useful answer.
    let unmerged = unmerged(worktree)?;
    if !unmerged.is_empty() {
        return Err(Error::Refused(format!(
            "{} path(s) are still unmerged in the sandbox:\n  {}\n\
             Resolve them and `git add` them there, then continue.",
            unmerged.len(),
            unmerged.join("\n  ")
        )));
    }

    let status = git::spawn(worktree, [subcommand, "--continue"], &[])?;
    classify(worktree, status)
}

/// Turns git's exit status plus the state it left behind into an [`Outcome`].
fn classify(worktree: &Path, status: ExitStatus) -> Result<Outcome> {
    // Exit status alone is not enough in either direction: an interactive
    // rebase that stops at `edit` or `break` exits 0 with work outstanding,
    // and a rebase that stops on a conflict exits non-zero with a sandbox
    // worth inspecting rather than a failure to report.
    if in_progress(worktree)?.is_some() {
        return Ok(Outcome::Stopped {
            conflicts: !git::run(worktree, ["ls-files", "--unmerged"])?.is_empty(),
        });
    }
    if status.success() {
        Ok(Outcome::Clean)
    } else {
        Ok(Outcome::Failed {
            code: status.code(),
        })
    }
}

/// Quotes a path for the shell git runs the sequence editor through.
///
/// Single quotes, with any embedded single quote closed, escaped and reopened
/// — the one form that needs no knowledge of what else the string contains.
/// A path that is not valid UTF-8 is rendered lossily; git would not be able
/// to run it either way, and a mangled path fails visibly.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::{Todo, sequence_editor, shell_quote};
    use std::path::{Path, PathBuf};

    fn command(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn todo() -> Todo {
        // Cargo.toml is only standing in for "a file that exists".
        Todo {
            file: PathBuf::from("Cargo.toml"),
            editor: PathBuf::from("/usr/local/bin/git-rehearse"),
        }
    }

    #[test]
    fn every_resumable_operation_knows_the_subcommand_that_carries_it_on() {
        use super::Operation;
        assert_eq!(Operation::Rebase.subcommand(), Some("rebase"));
        assert_eq!(Operation::CherryPick.subcommand(), Some("cherry-pick"));
        assert_eq!(Operation::Revert.subcommand(), Some("revert"));
        assert_eq!(Operation::Merge.subcommand(), Some("merge"));
        assert_eq!(
            Operation::Bisect.subcommand(),
            None,
            "a bisect advances on an answer only the user has, so there is nothing to run"
        );
    }

    #[test]
    fn paths_with_spaces_survive_the_shell() {
        assert_eq!(
            shell_quote(Path::new("/Users/someone/My Tools/git-rehearse")),
            "'/Users/someone/My Tools/git-rehearse'"
        );
    }

    #[test]
    fn a_quote_in_a_path_cannot_break_out_of_the_quoting() {
        let quoted = shell_quote(Path::new("/tmp/it's here/git-rehearse"));
        assert_eq!(quoted, r"'/tmp/it'\''s here/git-rehearse'");
    }

    #[test]
    fn an_interactive_rebase_gets_the_editor_command() {
        let editor = sequence_editor(&todo(), &command(&["rebase", "-i", "main"]))
            .expect("an interactive rebase accepts a todo");
        let editor = editor.to_string_lossy();
        assert!(editor.contains("__sequence-editor"), "{editor}");
        assert!(editor.contains("git-rehearse"), "{editor}");
        assert!(editor.contains("Cargo.toml"), "{editor}");
    }

    #[test]
    fn a_todo_for_something_that_is_not_a_rebase_is_refused() {
        let error = sequence_editor(&todo(), &command(&["merge", "feature"]))
            .expect_err("merge has no todo");
        assert!(
            error.to_string().contains("only applies to `rebase`"),
            "{error}"
        );
    }

    #[test]
    fn a_todo_for_a_non_interactive_rebase_is_refused_not_quietly_added() {
        let error =
            sequence_editor(&todo(), &command(&["rebase", "main"])).expect_err("no -i, no todo");
        assert!(error.to_string().contains("Add -i"), "{error}");
    }

    #[test]
    fn a_missing_todo_file_is_refused_before_git_runs() {
        let missing = Todo {
            file: PathBuf::from("no-such-todo-file"),
            ..todo()
        };
        let error = sequence_editor(&missing, &command(&["rebase", "-i", "main"]))
            .expect_err("a todo that is not there");
        assert!(error.to_string().contains("no such todo file"), "{error}");
    }
}
