//! git-rehearse — rehearse dangerous git commands in a shadow clone of your
//! real repository, inspect the outcome, then apply or discard.
//!
//! Pre-v0.1 scaffold: the user-facing command surface below is still a stub —
//! [`git_rehearse`] is where the implementation lives, and issue #8 wires the
//! two together with the stable exit codes. [`SCOPE.md`] is the authoritative
//! plan; nothing here is stable until v0.1.
//!
//! [`SCOPE.md`]: https://github.com/maximalcode/git-rehearse/blob/main/SCOPE.md

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use git_rehearse::execute::{self, SEQUENCE_EDITOR_ARG};

const USAGE: &str = "\
git-rehearse — rehearse dangerous git commands in a shadow clone of your repo

Pre-v0.1: nothing is implemented yet. SCOPE.md in this repository is the plan.

Planned surface:
  git rehearse rebase | merge | cherry-pick [args...]
  git rehearse -- <any git command>
  git rehearse list | show | apply | discard
";

/// What the process should do for a given argument list.
///
/// Exit-code semantics are reserved in SCOPE.md (0 clean, 2 conflicts,
/// 3 failed, 4 refused, 1 internal) and stabilise at v0.1 with #8; until then
/// the stub exits 1 for everything unimplemented.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    Version,
    /// git invoked us as its sequence editor, to install a prepared rebase
    /// todo over the one it generated. Not part of the command surface — git
    /// runs this, users do not.
    SequenceEditor {
        todo: PathBuf,
        target: PathBuf,
    },
    /// Anything we will not do, with the message to print.
    Refuse(String),
}

fn parse(args: &[String]) -> Action {
    let Some((first, rest)) = args.split_first() else {
        return Action::Help;
    };
    match first.as_str() {
        "--help" | "-h" => Action::Help,
        "--version" | "-V" => Action::Version,
        SEQUENCE_EDITOR_ARG => match rest {
            [todo, target] => Action::SequenceEditor {
                todo: PathBuf::from(todo),
                target: PathBuf::from(target),
            },
            _ => Action::Refuse(format!(
                "{SEQUENCE_EDITOR_ARG} is internal and takes exactly two paths"
            )),
        },
        other => Action::Refuse(format!(
            "'{other}' is not implemented yet (pre-v0.1). See --help or SCOPE.md."
        )),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Action::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Action::Version => {
            println!("git-rehearse {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Action::SequenceEditor { todo, target } => match execute::write_todo(&todo, &target) {
            Ok(()) => ExitCode::SUCCESS,
            // Non-zero matters here: it is what tells git to abort the rebase
            // rather than proceed with the todo it generated itself.
            Err(err) => {
                eprintln!("git-rehearse: {err}");
                ExitCode::FAILURE
            }
        },
        Action::Refuse(message) => {
            eprintln!("git-rehearse: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, parse};
    use std::path::PathBuf;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn no_args_and_help_flags_show_usage() {
        assert_eq!(parse(&args(&[])), Action::Help);
        assert_eq!(parse(&args(&["--help"])), Action::Help);
        assert_eq!(parse(&args(&["-h"])), Action::Help);
    }

    #[test]
    fn version_flags_report_version() {
        assert_eq!(parse(&args(&["--version"])), Action::Version);
        assert_eq!(parse(&args(&["-V"])), Action::Version);
    }

    #[test]
    fn planned_subcommands_are_refused_not_swallowed() {
        for command in ["rebase", "merge", "cherry-pick", "apply", "list"] {
            let Action::Refuse(message) = parse(&args(&[command])) else {
                panic!("{command} must be refused loudly");
            };
            assert!(message.contains(command), "{message}");
        }
    }

    #[test]
    fn git_can_invoke_us_as_its_sequence_editor() {
        assert_eq!(
            parse(&args(&[
                "__sequence-editor",
                "/tmp/todo",
                "/repo/.git/git-rebase-todo"
            ])),
            Action::SequenceEditor {
                todo: PathBuf::from("/tmp/todo"),
                target: PathBuf::from("/repo/.git/git-rebase-todo"),
            }
        );
    }

    #[test]
    fn a_malformed_internal_invocation_does_not_silently_succeed() {
        // Succeeding here would leave git rebasing against a todo nobody
        // asked for.
        let Action::Refuse(message) = parse(&args(&["__sequence-editor", "/tmp/todo"])) else {
            panic!("a one-argument sequence-editor call must be refused");
        };
        assert!(message.contains("exactly two paths"), "{message}");
    }
}
