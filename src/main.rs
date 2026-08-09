//! git-rehearse — rehearse dangerous git commands in a shadow clone of your
//! real repository, inspect the outcome, then apply or discard.
//!
//! The binary is deliberately thin: parse, run, print whatever went wrong, and
//! turn the result into one of the exit codes [`git_rehearse::cli`] fixes as
//! API. Everything else lives in the library, where it can be tested.
//!
//! [`SCOPE.md`]: https://github.com/maximalcode/git-rehearse/blob/main/SCOPE.md

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::process::ExitCode;

use git_rehearse::cli;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    let result = cli::parse(&args).and_then(|command| match std::env::current_dir() {
        Ok(cwd) => cli::run(command, &cwd, &mut output),
        Err(err) => Err(git_rehearse::Error::Spawn(err)),
    });

    if let Err(error) = &result {
        // Flush first: the report is on stdout and the failure is on stderr,
        // and a report that arrives after the error explaining it reads as a
        // different run.
        let _ = output.flush();
        eprintln!("git-rehearse: {error}");
    }
    ExitCode::from(cli::code_for(&result))
}
