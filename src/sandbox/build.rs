//! Bringing a shadow clone into existence, in the one order that works.
//!
//! Design principle 3 is this file's whole job: the sandbox is **disposable
//! and inert**. Inert means no remotes (an accidental `push` inside it must
//! have nowhere to go) and no hooks (the user's `pre-commit` must not fire for
//! a rehearsal). Disposable means it lives in the cache directory and can be
//! deleted at any moment without consequence.
//!
//! The step order in [`build`] is load-bearing and not obvious — config has to
//! be carried *before* the checkout, or `core.autocrlf` writes the wrong bytes
//! into the worktree — which is why it now lives here on its own rather than
//! interleaved with the directory-scanning code in [`super::store`].

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::meta::{META_SCHEMA, Meta};
use super::{HOOKS_DIR, Plan, Sandbox, WORKTREE_DIR};
use crate::{Error, Result, cache, git};

/// Builds a sandbox for `plan` under `cache_root`.
///
/// The steps, in order: claim a directory, clone with `--local`, promote every
/// branch to a local branch, strip the remote, point hooks at nothing, check
/// out, write `meta.json`. If any step fails the directory is removed again —
/// a half-built sandbox that still has a remote is worse than no sandbox.
///
/// `now_unix` is passed in rather than read so the whole lifecycle is testable
/// without waiting seven days for a prune.
///
/// # Errors
///
/// [`Error::Git`] if any git step fails (a checkout target that does not
/// exist, an unreadable repository), [`Error::Io`] for filesystem failures,
/// [`Error::Sandbox`] if the sandbox cannot be made inert.
pub fn create(cache_root: &Path, plan: &Plan, now_unix: u64) -> Result<Sandbox> {
    let repo_id = cache::repo_id(&plan.repo);
    let repo_dir = cache_root.join(&repo_id);
    fs::create_dir_all(&repo_dir).map_err(Error::io(&repo_dir))?;

    let (id, root) = claim_directory(&repo_dir, now_unix)?;
    match build(&root, plan, &repo_id, id.clone(), now_unix) {
        Ok(meta) => Ok(Sandbox { root, meta }),
        Err(err) => {
            // Best effort: if cleanup also fails, the original failure is the
            // one worth reporting, and prune will collect the leftovers.
            let _ = fs::remove_dir_all(&root);
            Err(err)
        }
    }
}

/// Claims the next free rehearsal directory and returns its id.
///
/// Exclusive directory creation *is* the id allocator: no lock file, no
/// randomness, and two rehearsals started in the same second cannot collide,
/// because whoever loses the `create_dir` race simply takes the next suffix.
fn claim_directory(repo_dir: &Path, now_unix: u64) -> Result<(String, PathBuf)> {
    for attempt in 0..100 {
        let id = format!("{now_unix}-{attempt:02}");
        let root = repo_dir.join(&id);
        match fs::create_dir(&root) {
            Ok(()) => return Ok((id, root)),
            // Taken already — try the next suffix.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(Error::Io(root, err)),
        }
    }
    Err(Error::Sandbox(format!(
        "{}: 100 rehearsals already started this second",
        repo_dir.display()
    )))
}

/// Everything that happens inside a claimed directory.
///
/// The order below is the contract. In particular `carry_config` comes before
/// `checkout`: the checkout materialises the worktree, and `core.autocrlf`
/// decides what bytes it materialises, so carrying that setting afterwards
/// would leave the sandbox holding line endings the real repository would
/// never have produced.
fn build(root: &Path, plan: &Plan, repo_id: &str, id: String, now_unix: u64) -> Result<Meta> {
    let hooks = root.join(HOOKS_DIR);
    fs::create_dir(&hooks).map_err(Error::io(&hooks))?;
    let worktree = root.join(WORKTREE_DIR);

    clone(root, &plan.repo, &hooks)?;
    promote_branches(&worktree)?;
    strip_remotes(&worktree)?;
    disable_hooks(&worktree, &hooks)?;
    carry_config(&plan.repo, &worktree)?;
    checkout(&worktree, &plan.checkout)?;

    let meta = Meta {
        schema: META_SCHEMA,
        id,
        repo_id: repo_id.to_owned(),
        repo_path: plan.repo.clone(),
        command: plan.command.clone(),
        checkout: plan.checkout.clone(),
        pre_state: plan.pre_state.clone(),
        created_unix: now_unix,
        status: super::Status::Fresh,
        result: None,
    };
    meta.write(root)?;
    Ok(meta)
}

/// `git clone --local --no-checkout` into `<root>/sandbox`.
fn clone(root: &Path, repo: &Path, hooks: &Path) -> Result<()> {
    // --local hardlinks the object store instead of copying it, so this is
    // fast and nearly free on disk even for a large repository. It is safe in
    // both directions because git objects are content-addressed and never
    // rewritten in place: a repack in either repo writes new files and unlinks
    // old ones. (Windows falls back to a copy — slower, still correct.)
    //
    // Deliberately NOT --shared/--reference: an alternates-based clone breaks
    // if the real repo runs `git gc` mid-rehearsal. Deliberately NOT
    // `git worktree`: worktrees share refs with the real repo, and refs are
    // exactly the mutation being sandboxed.
    //
    // --template points at our empty directory so git copies no sample hooks
    // in; --no-checkout because we check out deliberately, one step below.
    git::run(
        root,
        [
            OsString::from("clone"),
            OsString::from("--local"),
            OsString::from("--no-checkout"),
            git::flag_with_path("--template=", hooks),
            OsString::from("--"),
            repo.as_os_str().to_os_string(),
            OsString::from(WORKTREE_DIR),
        ],
    )?;
    Ok(())
}

/// Turns every remote-tracking branch into a local branch.
///
/// `git clone` creates exactly one local branch — whatever the source `HEAD`
/// pointed at — and parks every other branch under `refs/remotes/origin/*`.
/// Those refs vanish with the remote we are about to strip, and a rehearsal of
/// `rebase main` from a feature branch needs `main` to exist. So they are
/// promoted first, at the same SHAs.
///
/// One `update-ref --stdin -z` for the lot: NUL-delimited because a branch
/// name may legally contain a double quote, which the line-based format would
/// parse as the start of a quoted string.
///
/// Every command states what it expects to find: `update` with the SHA it
/// replaces for the one branch `clone` already created locally, and `create`
/// — which fails if the ref exists — for all the others. The promotion is then
/// a single transaction that refuses if the clone is not the shape we believe
/// it is, rather than overwriting something we did not predict.
///
/// Not `update` with an empty old value: in NUL-delimited mode git reads that
/// as "do not check", not as "must not exist".
fn promote_branches(worktree: &Path) -> Result<()> {
    let existing = git::refs(worktree, "refs/heads/", 2)?;
    let remote = git::refs(worktree, "refs/remotes/origin/", 3)?;

    let mut updates = String::new();
    for (name, sha) in &remote {
        // refs/remotes/origin/HEAD is the source's default-branch pointer, not
        // a branch of its own.
        if name == "HEAD" {
            continue;
        }
        // `write!` into the buffer rather than push_str(&format!(..)): no
        // intermediate String per branch, and writing to a String cannot fail.
        let _ = match existing.get(name) {
            Some(old) => write!(updates, "update refs/heads/{name}\0{sha}\0{old}\0"),
            None => write!(updates, "create refs/heads/{name}\0{sha}\0"),
        };
    }
    if updates.is_empty() {
        return Ok(());
    }
    git::run_with_stdin(worktree, ["update-ref", "--stdin", "-z"], Some(&updates))?;
    Ok(())
}

/// Removes every remote, then proves there are none left.
fn strip_remotes(worktree: &Path) -> Result<()> {
    let remotes = git::run(worktree, ["remote"])?;
    for remote in remotes.lines() {
        git::run(worktree, ["remote", "remove", remote])?;
    }
    // Verified rather than assumed: this is the difference between a sandbox
    // and a loaded gun. If a future git learns a remote we did not see, the
    // rehearsal must not start.
    let left = git::run(worktree, ["remote"])?;
    if left.is_empty() {
        Ok(())
    } else {
        Err(Error::Sandbox(format!(
            "sandbox still has remotes after stripping: {}",
            left.replace('\n', ", ")
        )))
    }
}

/// Points `core.hooksPath` at a directory we own and keep empty.
///
/// Not an empty config value: an empty `core.hooksPath` is a relative path,
/// not a documented spelling of "no hooks", and what it resolves to has
/// changed between git versions. An absolute path to an empty directory means
/// the same thing everywhere, and it is inspectable — a user who wonders
/// whether hooks ran can look.
fn disable_hooks(worktree: &Path, hooks: &Path) -> Result<()> {
    git::run(
        worktree,
        [
            OsString::from("config"),
            OsString::from("core.hooksPath"),
            hooks.as_os_str().to_os_string(),
        ],
    )?;
    Ok(())
}

/// Settings whose absence would make the rehearsal differ from the real
/// thing.
///
/// Deliberately a short list rather than "everything local". Copying the whole
/// config would carry `remote.*` and `core.hooksPath` straight back into a
/// sandbox that exists to have neither, and would keep doing so for every
/// setting git invents later. Each entry here earns its place by changing what
/// the rehearsed command *produces* or *shows*:
///
/// - `user.name` / `user.email`: a rebase rewrites the committer line on every
///   replayed commit, and those commits are what apply transplants.
/// - `core.autocrlf` / `core.eol`: they decide the bytes in the worktree the
///   command runs against, so a conflict can present differently without them.
/// - the signing group: whether to sign (`commit.gpgsign`, `tag.gpgsign`),
///   what to sign with (`user.signingkey`, `gpg.format`) and what does the
///   signing (`gpg.*.program`). Because apply is a **ref transplant**, the
///   commits made in the sandbox *become* the commits in the real repository
///   — so without these, rehearsing a rebase in a repository that signs
///   locally would replace signed history with unsigned history, silently.
///   That is exactly the class of surprise this tool exists to prevent.
///
/// Verification settings (`gpg.ssh.allowedSignersFile`, `gpg.minTrustLevel`)
/// are deliberately absent: they change whether git *believes* a signature,
/// never whether it produces one, and nothing in a rehearsal verifies.
///
/// A repository that signs but whose key the sandbox cannot reach now fails
/// the rehearsal outright rather than producing unsigned commits. That is the
/// right way round: a loud exit 3 is recoverable, a quiet signature downgrade
/// is not.
///
/// (`.gitattributes` needs no carrying — it is tracked content, so the clone
/// already has it.)
const CARRIED_CONFIG: &[&str] = &[
    "user.name",
    "user.email",
    "core.autocrlf",
    "core.eol",
    "commit.gpgsign",
    "tag.gpgsign",
    "user.signingkey",
    "gpg.format",
    "gpg.program",
    "gpg.openpgp.program",
    "gpg.ssh.program",
    "gpg.x509.program",
];

/// Copies the settings above from the real repository into the sandbox.
///
/// `git clone` does not copy `.git/config`, so anything set *locally* — a work
/// checkout's identity, a per-repo line-ending policy — is simply absent from
/// the sandbox. Principle 1 says the sandbox runs the user's git with the
/// user's config, and an author line or a worktree that differs from the real
/// repository's breaks that where it shows most.
///
/// Read with `--get`, which resolves local over global over system, so what
/// lands in the sandbox is exactly what the real repository would have used.
/// A setting configured nowhere is left unset rather than invented: git then
/// behaves in the sandbox exactly as it would have at home, including refusing
/// to commit without an identity.
fn carry_config(repo: &Path, worktree: &Path) -> Result<()> {
    for key in CARRIED_CONFIG {
        if let Ok(value) = git::run(repo, ["config", "--get", key])
            && !value.is_empty()
        {
            git::run(worktree, ["config", key, &value])?;
        }
    }
    Ok(())
}

/// Checks out what the real repository had checked out.
fn checkout(worktree: &Path, target: &super::Checkout) -> Result<()> {
    // The trailing `--` keeps a branch named like a file from being read as a
    // pathspec.
    match target {
        super::Checkout::Branch(name) => git::run(worktree, ["checkout", "--quiet", name, "--"])?,
        super::Checkout::Detached(sha) => {
            git::run(worktree, ["checkout", "--quiet", "--detach", sha, "--"])?
        }
    };
    Ok(())
}
