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

    let result = cli::parse(&args).and_then(|parsed| match std::env::current_dir() {
        Ok(cwd) => cli::run(parsed, &cwd, &mut output),
        Err(err) => Err(git_rehearse::Error::Spawn(err)),
    });
    let code = cli::code_for(&result);

    if let Err(error) = &result {
        // A caller that asked for JSON gets JSON on every exit path, including
        // this one: one that parses stdout on success and meets English on
        // failure has to parse English anyway. Read off the arguments rather
        // than the parse, because parsing is one of the things that can fail
        // here. The human line still goes to stderr, which nobody parses.
        if cli::wants_json(&args) {
            let _ = cli::write_failure(&error.to_string(), code, &mut output);
        }
        // Flush first: the report is on stdout and the failure is on stderr,
        // and a report that arrives after the error explaining it reads as a
        // different run.
        let _ = output.flush();
        eprintln!("git-rehearse: {error}");
    }
    ExitCode::from(code)
}
