//! The thin layer over the user's real `git`.
//!
//! Design principle 1 — *real git executes, always* — means this crate never
//! reimplements git behaviour, so this module stays deliberately small: run a
//! command, capture what it said, fail loudly with git's own words. Anything
//! that reasons about the output belongs in a module with unit tests, not
//! here.
//!
//! Everything in this module captures stdio. The interactive path (running the
//! rehearsed command against the user's editor and terminal) is a separate
//! concern and does not belong here.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::{Error, Result};

/// Runs `git -C <dir> <args...>` and returns its stdout, trailing newline
/// trimmed.
///
/// Stdin is `/dev/null`: a git subprocess that decides to ask a question here
/// would hang forever instead, and every caller in this module is
/// non-interactive by construction.
///
/// # Errors
///
/// [`Error::Spawn`] if git is not runnable at all, [`Error::Git`] with git's
/// stderr if it ran and exited non-zero.
pub fn run<I, S>(dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_stdin(dir, args, None)
}

/// Runs [`run`] with additional environment variables for git.
///
/// This is used by disposable probes that need a temporary worktree or an
/// object alternate; the real repository's refs and index remain untouched.
pub fn run_with_env<I, S>(dir: &Path, args: I, env: &[(&str, OsString)]) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_stdin_and_env(dir, args, None, env)
}

/// Runs git with the environment controls that can redirect or alter a
/// disposable probe removed first. The caller's PATH, HOME and repository
/// configuration remain available, but command-scoped config, alternate
/// indexes, object stores and worktrees cannot leak into the probe.
pub fn run_with_clean_env<I, S>(dir: &Path, args: I, env: &[(&str, OsString)]) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_stdin_and_env_clean(dir, args, None, env)
}

/// Runs `git -C <dir> <args...>` and returns stdout without decoding it.
///
/// Git's NUL-delimited config and path protocols are byte protocols. Callers
/// that need to preserve those values must not pass through [`String`], whose
/// UTF-8 conversion would replace invalid bytes.
pub fn run_bytes<I, S>(dir: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(Error::Spawn)?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(Error::Git {
        args: describe(&args),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// As [`run_with_clean_env`], retaining stdout bytes for Git's NUL-delimited
/// path protocols.
pub fn run_bytes_with_clean_env<I, S>(
    dir: &Path,
    args: I,
    env: &[(&str, OsString)],
) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(&args);
    for (key, _) in std::env::vars_os() {
        if probe_environment_key(&key) {
            command.env_remove(key);
        }
    }
    let output = command
        .envs(env.iter().map(|(key, value)| (*key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(Error::Spawn)?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(Error::Git {
        args: describe(&args),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// [`run`], with `input` fed to git's stdin.
///
/// Used for `update-ref --stdin`, where the alternative is one process per
/// ref. The input is written and the pipe closed before the output is read;
/// git's stdin-driven commands consume their whole input before writing
/// anything substantial, so this cannot deadlock on the volumes we send.
///
/// # Errors
///
/// As [`run`], plus [`Error::Spawn`] if the pipe to git breaks mid-write.
pub fn run_with_stdin<I, S>(dir: &Path, args: I, input: Option<&str>) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_stdin_and_env(dir, args, input, &[])
}

fn run_with_stdin_and_env<I, S>(
    dir: &Path,
    args: I,
    input: Option<&str>,
    env: &[(&str, OsString)],
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_stdin_and_env_impl(dir, args, input, env, false)
}

fn run_with_stdin_and_env_clean<I, S>(
    dir: &Path,
    args: I,
    input: Option<&str>,
    env: &[(&str, OsString)],
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_stdin_and_env_impl(dir, args, input, env, true)
}

fn run_with_stdin_and_env_impl<I, S>(
    dir: &Path,
    args: I,
    input: Option<&str>,
    env: &[(&str, OsString)],
    clean_git_environment: bool,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();

    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(&args);
    if clean_git_environment {
        for (key, _) in std::env::vars_os() {
            if probe_environment_key(&key) {
                command.env_remove(key);
            }
        }
    }
    let mut child = command
        .envs(env.iter().map(|(key, value)| (*key, value)))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::Spawn)?;

    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Sandbox("git stdin was not available".to_owned()))?;
        stdin.write_all(input.as_bytes()).map_err(Error::Spawn)?;
        // Dropped here on purpose: git waits for EOF, not for us.
        drop(stdin);
    }

    let output = child.wait_with_output().map_err(Error::Spawn)?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned());
    }
    Err(Error::Git {
        args: describe(&args),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn probe_environment_key(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    [
        "GIT_CONFIG_",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_GRAFT_FILE",
        "GIT_REPLACE_REF_BASE",
        "GIT_SHALLOW_FILE",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_TEMPLATE_DIR",
        "GIT_ATTR_",
    ]
    .iter()
    .any(|prefix| key == *prefix || key.starts_with(prefix))
}

/// Runs git in `dir` with the parent's stdin, stdout and stderr, and returns
/// its exit status.
///
/// The one place a rehearsed command meets the user's terminal. `rebase -i`
/// has to be able to open the user's editor and draw on the user's screen —
/// principle 1 says the sandbox runs *their* git with *their* environment, and
/// capturing the output would break that on purpose.
///
/// `env` is layered on top of the inherited environment, not a replacement
/// for it.
///
/// # Errors
///
/// [`Error::Spawn`] if git cannot be started or waited on. A non-zero exit is
/// *not* an error here: for a rehearsal it is a result, and classifying it is
/// the caller's job.
pub fn spawn<I, S>(dir: &Path, args: I, env: &[(&str, OsString)]) -> Result<ExitStatus>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    spawn_with(dir, args, env, Chatter::Inherit)
}

/// Where git's own stdout goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chatter {
    /// Straight through to ours. The user is watching git work, and principle 1
    /// says they see their git behaving exactly as it does anywhere else.
    Inherit,
    /// Forwarded to *our* stderr instead.
    ///
    /// For `--json`, where stdout carries one document and nothing else. Git
    /// writes `Auto-merging …` and `CONFLICT …` to its stdout, and inheriting
    /// that puts two lines of English in front of the document — enough to
    /// break any caller that parses the stream. Forwarded rather than dropped:
    /// it is still what git said, and it still belongs in a log.
    ToStderr,
}

/// [`spawn`], with a say in where git's stdout goes.
///
/// # Errors
///
/// As [`spawn`], plus [`Error::Spawn`] if git's output cannot be read back.
pub fn spawn_with<I, S>(
    dir: &Path,
    args: I,
    env: &[(&str, OsString)],
    chatter: Chatter,
) -> Result<ExitStatus>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.stdin(Stdio::inherit()).stderr(Stdio::inherit());

    if chatter == Chatter::Inherit {
        return command
            .stdout(Stdio::inherit())
            .status()
            .map_err(Error::Spawn);
    }

    // Only stdout is piped, and it is drained before the wait — git's stderr
    // still goes straight out, so there is no second pipe to fill and deadlock
    // against.
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(Error::Spawn)?;
    if let Some(mut chatter) = child.stdout.take() {
        let mut said = Vec::new();
        chatter.read_to_end(&mut said).map_err(Error::Spawn)?;
        io::stderr().write_all(&said).map_err(Error::Spawn)?;
    }
    child.wait().map_err(Error::Spawn)
}

/// The refs under `pattern` as `name -> sha`, with `strip` leading components
/// removed from each name.
///
/// `refs(dir, "refs/heads/", 0)` gives full ref names, which is what a
/// pre-state snapshot records; `strip = 2` gives bare branch names.
///
/// # Errors
///
/// As [`run`].
pub fn refs(dir: &Path, pattern: &str, strip: u8) -> Result<BTreeMap<String, String>> {
    let listing = run(
        dir,
        [
            "for-each-ref".to_owned(),
            format!("--format=%(objectname) %(refname:lstrip={strip})"),
            pattern.to_owned(),
        ],
    )?;
    Ok(listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, name)| !name.is_empty())
        .map(|(sha, name)| (name.to_owned(), sha.to_owned()))
        .collect())
}

/// Renders an argument list for an error message.
///
/// Lossy on purpose: this string is for a human reading a failure, never for
/// re-execution, so a path that is not valid UTF-8 should still be legible
/// rather than turn the error into a different error.
fn describe(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolves a path to its canonical form, in a spelling git accepts.
///
/// Canonical because the cache directory name is derived from the repository
/// path, and `~/dev/app` and `~/dev/../dev/app` must not become two rehearsal
/// histories — on macOS that also resolves `/var` to `/private/var`.
///
/// The second half matters only on Windows, where [`Path::canonicalize`]
/// returns an extended-length path (`\\?\C:\...`). Git reads a leading `\\`
/// as a host and answers `hostname contains invalid characters`, so a path in
/// that form cannot be handed to `git clone`. The prefix is stripped for
/// ordinary drive paths; a genuine UNC path keeps its `\\` because there the
/// leading slashes mean what git thinks they mean.
///
/// # Errors
///
/// [`Error::Io`] if the path cannot be resolved.
pub fn canonicalize(path: &Path) -> Result<PathBuf> {
    let resolved = path.canonicalize().map_err(Error::io(path))?;
    Ok(git_readable(resolved))
}

#[cfg(windows)]
fn git_readable(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return path;
    };
    let Prefix::VerbatimDisk(letter) = prefix.kind() else {
        // VerbatimUNC and the rest keep their leading slashes: those really
        // are host-shaped paths, and git treats them as such correctly.
        return path;
    };
    let text = path.to_string_lossy();
    // r"\\?\C:\dir" -> r"C:\dir"; the first six bytes are ASCII, always.
    PathBuf::from(format!("{}:{}", letter as char, &text[6..]))
}

#[cfg(not(windows))]
fn git_readable(path: PathBuf) -> PathBuf {
    path
}

/// Appends a path to a `--flag=` prefix without going through `String`.
///
/// `format!("--template={}", path.display())` would corrupt a path that is not
/// valid UTF-8; git takes bytes, so we hand it bytes.
#[must_use]
pub fn flag_with_path(prefix: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(prefix);
    arg.push(path);
    arg
}

#[cfg(test)]
mod tests {
    use super::{describe, flag_with_path, run};
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn a_failing_git_command_reports_gits_own_words() {
        // Chosen because it fails identically whether or not the working
        // directory happens to be a git repository — a unit test should not
        // depend on where the test runner was launched from.
        let dir = std::env::current_dir().expect("a working directory");
        let err = run(
            &dir,
            ["config", "--file", "/nonexistent/gitconfig", "--get", "a.b"],
        )
        .expect_err("git must reject an unreadable config file");
        let message = err.to_string();
        assert!(message.contains("config"), "{message}");
        assert!(
            message.contains("/nonexistent/gitconfig"),
            "git's stderr must survive into the error: {message}"
        );
    }

    #[test]
    fn stdout_comes_back_without_its_trailing_newline() {
        let dir = std::env::current_dir().expect("a working directory");
        let version = run(&dir, ["--version"]).expect("git --version");
        assert!(version.starts_with("git version"), "{version}");
        assert!(!version.ends_with('\n'));
    }

    #[test]
    fn arguments_render_readably_for_error_messages() {
        let args: Vec<OsString> = ["checkout", "--detach", "deadbeef"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(describe(&args), "checkout --detach deadbeef");
    }

    #[test]
    fn path_flags_keep_the_path_intact() {
        let arg = flag_with_path("--template=", Path::new("/tmp/no hooks"));
        assert_eq!(arg, OsString::from("--template=/tmp/no hooks"));
    }
}
