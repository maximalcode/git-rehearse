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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Turns git's exit status plus the state it left behind into an [`Outcome`].
fn classify(worktree: &Path, status: ExitStatus) -> Result<Outcome> {
    // Exit status alone is not enough in either direction: an interactive
    // rebase that stops at `edit` or `break` exits 0 with work outstanding,
    // and a rebase that stops on a conflict exits non-zero with a sandbox
    // worth inspecting rather than a failure to report.
    if operation_in_progress(worktree)? {
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

/// Whether git left an operation half-finished in the sandbox.
fn operation_in_progress(worktree: &Path) -> Result<bool> {
    let git_dir = PathBuf::from(git::run(worktree, ["rev-parse", "--absolute-git-dir"])?);
    // The states `git status` itself reports as an operation in progress.
    Ok([
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
    ]
    .iter()
    .any(|marker| git_dir.join(marker).exists()))
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
