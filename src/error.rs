//! One error type for the whole crate.
//!
//! Every variant carries what a user needs to act on it — which path, which
//! git invocation, what git said — because design principle 5 is *refuse
//! loudly rather than guess*, and a refusal that does not say what happened is
//! a guess wearing a stack trace.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The crate result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong below the CLI layer.
#[derive(Debug)]
pub enum Error {
    /// A filesystem operation failed, on this path.
    Io(PathBuf, io::Error),
    /// `git` could not be spawned at all — not on `PATH`, or not executable.
    Spawn(io::Error),
    /// `git` ran and exited non-zero.
    Git {
        /// The arguments passed to git, for the message. Never the env.
        args: String,
        /// Exit status, or `None` if git was killed by a signal.
        code: Option<i32>,
        /// What git wrote to stderr, trimmed.
        stderr: String,
    },
    /// A `meta.json` could not be written or parsed.
    Meta(PathBuf, serde_json::Error),
    /// No usable cache directory could be determined from the environment.
    NoCacheDir,
    /// The sandbox could not be built for a structural reason of our own.
    Sandbox(String),
}

impl Error {
    /// Maps an [`io::Error`] onto the path it happened to.
    ///
    /// `fs::create_dir(&p).map_err(Error::io(&p))?` reads better than the
    /// closure written out at every call site, and it means no I/O error in
    /// this crate reaches a user without saying *which file*.
    pub(crate) fn io(path: &Path) -> impl FnOnce(io::Error) -> Self + use<> {
        let path = path.to_path_buf();
        move |source| Self::Io(path, source)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, source) => write!(f, "{}: {source}", path.display()),
            Self::Spawn(source) => write!(f, "could not run git: {source}"),
            Self::Git { args, code, stderr } => {
                let status =
                    code.map_or_else(|| "killed by signal".to_owned(), |c| format!("exit {c}"));
                write!(f, "git {args} failed ({status})")?;
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Meta(path, source) => write!(f, "{}: {source}", path.display()),
            Self::NoCacheDir => f.write_str(
                "no cache directory: set GIT_REHEARSE_CACHE_DIR, XDG_CACHE_HOME, or HOME",
            ),
            Self::Sandbox(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(_, source) | Self::Spawn(source) => Some(source),
            Self::Meta(_, source) => Some(source),
            Self::Git { .. } | Self::NoCacheDir | Self::Sandbox(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use std::io;
    use std::path::Path;

    #[test]
    fn io_errors_name_the_path_they_happened_to() {
        let err = Error::io(Path::new("/nowhere/meta.json"))(io::Error::new(
            io::ErrorKind::NotFound,
            "no such file",
        ));
        assert!(err.to_string().contains("/nowhere/meta.json"));
        assert!(err.to_string().contains("no such file"));
    }

    #[test]
    fn git_failures_quote_git_rather_than_paraphrase_it() {
        let err = Error::Git {
            args: "checkout --detach deadbeef".to_owned(),
            code: Some(128),
            stderr: "fatal: reference is not a tree: deadbeef".to_owned(),
        };
        let message = err.to_string();
        assert!(message.contains("exit 128"), "{message}");
        assert!(message.contains("reference is not a tree"), "{message}");
    }

    #[test]
    fn signal_deaths_are_not_reported_as_exit_zero() {
        let err = Error::Git {
            args: "clone".to_owned(),
            code: None,
            stderr: String::new(),
        };
        assert!(err.to_string().contains("killed by signal"));
    }
}
