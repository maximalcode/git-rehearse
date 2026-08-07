//! git-rehearse — rehearse dangerous git commands in a shadow clone of your
//! real repository, inspect the outcome, then apply or discard.
//!
//! Pre-v0.1 scaffold: the CLI surface below is a stub. [`SCOPE.md`] in this
//! repository is the authoritative plan; nothing here is stable until v0.1.
//!
//! [`SCOPE.md`]: https://github.com/maximalcode/git-rehearse/blob/main/SCOPE.md

#![forbid(unsafe_code)]

use std::process::ExitCode;

const USAGE: &str = "\
git-rehearse — rehearse dangerous git commands in a shadow clone of your repo

Pre-v0.1: nothing is implemented yet. SCOPE.md in this repository is the plan.

Planned surface:
  git rehearse rebase | merge | cherry-pick [args...]
  git rehearse -- <any git command>
  git rehearse list | show | apply | discard
";

/// What the process should do for a given first argument.
///
/// Exit-code semantics are reserved in SCOPE.md (0 clean, 2 conflicts,
/// 3 failed, 4 refused, 1 internal) and stabilise at v0.1; until then the
/// stub exits 1 for everything unimplemented.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    Version,
    Unimplemented(String),
}

fn parse(first: Option<&str>) -> Action {
    match first {
        None | Some("--help" | "-h") => Action::Help,
        Some("--version" | "-V") => Action::Version,
        Some(other) => Action::Unimplemented(other.to_owned()),
    }
}

fn main() -> ExitCode {
    let first = std::env::args().nth(1);
    match parse(first.as_deref()) {
        Action::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Action::Version => {
            println!("git-rehearse {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Action::Unimplemented(cmd) => {
            eprintln!(
                "git-rehearse: '{cmd}' is not implemented yet (pre-v0.1). \
                 See --help or SCOPE.md."
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, parse};

    #[test]
    fn no_args_and_help_flags_show_usage() {
        assert_eq!(parse(None), Action::Help);
        assert_eq!(parse(Some("--help")), Action::Help);
        assert_eq!(parse(Some("-h")), Action::Help);
    }

    #[test]
    fn version_flags_report_version() {
        assert_eq!(parse(Some("--version")), Action::Version);
        assert_eq!(parse(Some("-V")), Action::Version);
    }

    #[test]
    fn planned_subcommands_are_refused_not_swallowed() {
        for cmd in ["rebase", "merge", "cherry-pick", "apply", "list"] {
            assert_eq!(parse(Some(cmd)), Action::Unimplemented(cmd.to_owned()));
        }
    }
}
