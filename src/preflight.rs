//! What we learn about the real repository before touching it — and
//! everything we refuse to rehearse.
//!
//! Two jobs, and they belong together because they read the same repository
//! once:
//!
//! 1. **The snapshot.** Every branch and where `HEAD` is, recorded before the
//!    sandbox exists. Apply re-reads this and refuses if anything moved, which
//!    is the only thing standing between a rehearsal and a commit somebody
//!    else made while you were reading the report.
//! 2. **The refusals.** Design principle 5 — *refuse loudly rather than
//!    guess*. Every message here is product surface: it is read by someone
//!    mid-rebase who wants to know what is in the way and what to do about it,
//!    so it says both, in that order, and never more than that.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::sandbox::{Checkout, Plan};
use crate::{Error, Result, git};

/// The key the pre-state records `HEAD`'s commit under, alongside the full
/// `refs/heads/*` names.
pub const HEAD_KEY: &str = "HEAD";

/// How many changed paths a refusal names before it says "and N more".
const PATHS_IN_MESSAGE: usize = 3;

/// A repository that passed preflight, and what was true of it at that moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    /// Canonicalised worktree root.
    pub repo: PathBuf,
    /// What `HEAD` pointed at — the sandbox reproduces this.
    pub checkout: Checkout,
    /// Every `refs/heads/*` plus [`HEAD_KEY`], as `name -> sha`.
    pub pre_state: BTreeMap<String, String>,
}

impl Preflight {
    /// Turns the snapshot into a plan for [`crate::sandbox::create`].
    #[must_use]
    pub fn into_plan(self, command: Vec<String>) -> Plan {
        Plan {
            repo: self.repo,
            command,
            checkout: self.checkout,
            pre_state: self.pre_state,
        }
    }
}

/// Inspects the repository containing `cwd`.
///
/// # Errors
///
/// [`Error::Refused`] — with a message meant to be printed as-is — if the
/// repository is one v0.1 will not rehearse. [`Error::Git`] or [`Error::Io`]
/// if the inspection itself fails.
pub fn run(cwd: &Path) -> Result<Preflight> {
    let repo = locate(cwd)?;

    // Order matters. The structural refusals come first because a user cannot
    // fix them in a minute: being told to commit your changes, and only then
    // being told that submodules are unsupported anyway, wastes someone's
    // afternoon.
    refuse_if_shallow(&repo)?;
    refuse_if_multiple_worktrees(&repo)?;
    refuse_if_submodules(&repo)?;
    refuse_if_lfs(&repo)?;

    let (checkout, head_sha) = head(&repo)?;
    refuse_if_dirty(&repo)?;

    Ok(Preflight {
        repo: repo.clone(),
        checkout,
        pre_state: snapshot(&repo, head_sha)?,
    })
}

/// The canonical worktree root containing `cwd`.
fn locate(cwd: &Path) -> Result<PathBuf> {
    let facts = match git::run(
        cwd,
        ["rev-parse", "--is-bare-repository", "--is-inside-work-tree"],
    ) {
        Ok(facts) => facts,
        // git ran and said no. Anything else — git missing, git killed — is
        // our problem, not the user's repository, and must not be dressed up
        // as a refusal.
        Err(Error::Git { .. }) => {
            return Err(refused(format!(
                "not a git repository: {}\n\
                 Run git-rehearse from inside the repository you want to rehearse against.",
                cwd.display()
            )));
        }
        Err(other) => return Err(other),
    };

    let mut facts = facts.lines();
    let bare = facts.next() == Some("true");
    let inside_worktree = facts.next() == Some("true");

    if bare {
        return Err(refused(
            "this is a bare repository.\n\
             A rehearsal runs the real command in a real worktree, and a bare repository has \
             none — run git-rehearse from a normal checkout."
                .to_owned(),
        ));
    }
    if !inside_worktree {
        return Err(refused(format!(
            "{} is inside the .git directory, not the worktree.\n\
             Run git-rehearse from the working tree.",
            cwd.display()
        )));
    }

    let top = git::run(cwd, ["rev-parse", "--show-toplevel"])?;
    let repo = PathBuf::from(top);
    // git prints an absolute path but not necessarily a resolved one, and the
    // cache directory name is derived from this — /var/... and /private/var/...
    // must not become two rehearsal histories for one repository.
    repo.canonicalize().map_err(Error::io(&repo))
}

fn refuse_if_shallow(repo: &Path) -> Result<()> {
    if git::run(repo, ["rev-parse", "--is-shallow-repository"])? == "true" {
        return Err(refused(
            "this is a shallow clone.\n\
             The rehearsal would inherit its missing history, so the result could differ from \
             what the same command does in a complete clone — run `git fetch --unshallow` first."
                .to_owned(),
        ));
    }
    Ok(())
}

fn refuse_if_multiple_worktrees(repo: &Path) -> Result<()> {
    let listing = git::run(repo, ["worktree", "list", "--porcelain"])?;
    let count = listing
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count();
    if count > 1 {
        return Err(refused(format!(
            "this repository has {count} worktrees (see `git worktree list`).\n\
             Applying a rehearsal moves branches, and a branch checked out in another worktree \
             would be moved out from under it — v0.1 will not risk that. Remove the extra \
             worktrees, or rehearse elsewhere."
        )));
    }
    Ok(())
}

fn refuse_if_submodules(repo: &Path) -> Result<()> {
    if !git::run(repo, ["submodule", "status"])?.is_empty() {
        return Err(refused(
            "this repository has submodules.\n\
             A local clone does not bring them along, so the sandbox would run your command \
             against a checkout that is missing them and the report would be misleading. \
             Submodules are not supported in v1."
                .to_owned(),
        ));
    }
    Ok(())
}

fn refuse_if_lfs(repo: &Path) -> Result<()> {
    let listing = git::run(
        repo,
        ["ls-files", "-z", "--", ".gitattributes", "*/.gitattributes"],
    )?;
    for name in listing.split('\0').filter(|name| !name.is_empty()) {
        let path = repo.join(name);
        // Unreadable (sparse checkout, permissions) means we cannot prove LFS
        // is in use, and preflight refuses on evidence, not on suspicion.
        let Ok(attributes) = fs::read_to_string(&path) else {
            continue;
        };
        if attributes.contains("filter=lfs") {
            return Err(refused(format!(
                "this repository uses Git LFS ({name} sets filter=lfs).\n\
                 A local clone does not copy the LFS object store, so the sandbox would check \
                 out pointer files instead of your content. LFS is not supported in v1."
            )));
        }
    }
    Ok(())
}

fn refuse_if_dirty(repo: &Path) -> Result<()> {
    // Untracked files are deliberately not a refusal: `git rebase` itself runs
    // happily with untracked files present, and principle 5 refuses where git
    // refuses — being stricter than git is its own kind of surprise.
    let status = git::run(repo, ["status", "--porcelain", "--untracked-files=no"])?;
    if status.is_empty() {
        return Ok(());
    }
    Err(refused(dirty_message(&status)))
}

/// Builds the dirty-worktree refusal from `git status --porcelain` output.
fn dirty_message(status: &str) -> String {
    let paths: Vec<&str> = status
        .lines()
        // "XY path" — the two status columns and their space are always ASCII.
        .filter_map(|line| line.get(3..))
        .collect();
    let mut named = paths
        .iter()
        .take(PATHS_IN_MESSAGE)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(rest) = paths.len().checked_sub(PATHS_IN_MESSAGE).filter(|n| *n > 0) {
        let _ = write!(named, " and {rest} more");
    }
    format!(
        "you have uncommitted changes: {named}.\n\
         v0.1 rehearses committed history only, so the rehearsal would not include them — \
         commit or stash first. (Rehearsing a dirty worktree is planned for v1.x.)"
    )
}

/// Where `HEAD` is, as something the sandbox can reproduce, plus its commit.
fn head(repo: &Path) -> Result<(Checkout, String)> {
    let sha = match git::run(repo, ["rev-parse", "--verify", "--quiet", "HEAD"]) {
        Ok(sha) => sha,
        Err(Error::Git { .. }) => {
            return Err(refused(
                "this repository has no commits yet.\n\
                 There is no history here to rehearse against."
                    .to_owned(),
            ));
        }
        Err(other) => return Err(other),
    };
    match git::run(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(branch) => Ok((Checkout::Branch(branch), sha)),
        // symbolic-ref --quiet exits non-zero precisely when HEAD is detached.
        Err(Error::Git { .. }) => Ok((Checkout::Detached(sha.clone()), sha)),
        Err(other) => Err(other),
    }
}

/// Every branch, plus where `HEAD` was.
///
/// Branches and `HEAD` only: those are the refs an apply moves, so those are
/// the refs whose movement in the meantime invalidates the rehearsal. A tag
/// that appears while you read the report changes nothing about whether the
/// rehearsed result is still the one you were shown.
fn snapshot(repo: &Path, head_sha: String) -> Result<BTreeMap<String, String>> {
    let mut refs = git::refs(repo, "refs/heads/", 0)?;
    refs.insert(HEAD_KEY.to_owned(), head_sha);
    Ok(refs)
}

fn refused(message: String) -> Error {
    Error::Refused(message)
}

#[cfg(test)]
mod tests {
    use super::dirty_message;

    #[test]
    fn a_dirty_worktree_is_told_what_is_dirty() {
        let message = dirty_message(" M src/main.rs\nM  Cargo.toml");
        assert!(message.contains("src/main.rs"), "{message}");
        assert!(message.contains("Cargo.toml"), "{message}");
        assert!(message.contains("commit or stash"), "{message}");
    }

    #[test]
    fn a_very_dirty_worktree_is_summarised_not_dumped() {
        let status = (0..12)
            .map(|n| format!(" M file{n}.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        let message = dirty_message(&status);
        assert!(
            message.contains("file0.rs, file1.rs, file2.rs"),
            "{message}"
        );
        assert!(message.contains("and 9 more"), "{message}");
        assert!(!message.contains("file11.rs"), "{message}");
    }

    #[test]
    fn every_refusal_says_what_is_wrong_then_what_to_do() {
        // The two-line shape is the contract the CLI prints against, and the
        // reason these messages read as help rather than as an error dump.
        let message = dirty_message(" M src/main.rs");
        let (problem, remedy) = message.split_once('\n').expect("two lines");
        assert!(problem.ends_with('.'), "{problem}");
        assert!(!remedy.is_empty());
    }
}
