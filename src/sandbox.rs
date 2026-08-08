//! The shadow clone: creating one, describing one, throwing one away.
//!
//! A rehearsal directory is self-describing — `meta.json` next to the clone
//! holds everything a later invocation needs to report on it or apply it, so
//! nothing about a rehearsal lives only in memory.
//!
//! Design principle 3 is the whole job of this module: the sandbox is
//! **disposable and inert**. Inert means no remotes (an accidental `push`
//! inside it must have nowhere to go) and no hooks (the user's `pre-commit`
//! must not fire for a rehearsal). Disposable means it lives in the cache
//! directory, is deleted immediately on discard, and is pruned by age.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::execute::Outcome;
use crate::{Error, Result, cache, git};

/// Version of the `meta.json` document. Bump on any incompatible change; a
/// build that meets an unfamiliar schema refuses the rehearsal rather than
/// half-reading it.
pub const META_SCHEMA: u32 = 1;

/// How long a kept rehearsal survives without being touched: seven days.
pub const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

const META_FILE: &str = "meta.json";
const META_TMP: &str = "meta.json.tmp";
const WORKTREE_DIR: &str = "sandbox";
const HOOKS_DIR: &str = "no-hooks";

/// What the sandbox should have checked out when the rehearsed command runs.
///
/// Captured by preflight from the real repository's `HEAD`, and recorded so a
/// report can say what the rehearsal started from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "target")]
pub enum Checkout {
    /// The real repo was on a branch; the sandbox checks out the same one.
    Branch(String),
    /// The real repo had a detached `HEAD`; the sandbox detaches at the same
    /// commit.
    Detached(String),
}

/// Whether a rehearsal is still in flight or has been deliberately kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Created by the current run; discarded unless the user says otherwise.
    Fresh,
    /// Kept on purpose, listed by `git rehearse list` until it ages out.
    Kept,
}

/// What a caller must decide before a sandbox can exist.
///
/// Every field is produced by preflight (issue #3) against the real
/// repository; this module takes them as given and never inspects the real
/// repo itself.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Canonicalised worktree root of the real repository. Canonical because
    /// `~/dev/app` and `~/dev/../dev/app` must not get two cache directories.
    pub repo: PathBuf,
    /// The command being rehearsed, as the user wrote it (`["rebase", "-i",
    /// "main"]`). Recorded for the report; not run by this module.
    pub command: Vec<String>,
    /// What to check out in the sandbox.
    pub checkout: Checkout,
    /// Every ref in the real repository at snapshot time, `refname -> sha`.
    /// Stored verbatim: apply re-reads it to prove nothing moved underneath
    /// the user, so it is evidence, not a cache.
    pub pre_state: BTreeMap<String, String>,
}

/// The self-describing contents of `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// See [`META_SCHEMA`].
    pub schema: u32,
    /// Rehearsal id, unique within the repository's cache directory.
    pub id: String,
    /// Cache directory name for the repository, see [`cache::repo_id`].
    pub repo_id: String,
    /// Where the real repository is.
    pub repo_path: PathBuf,
    /// The command being rehearsed.
    pub command: Vec<String>,
    /// What the sandbox has checked out.
    pub checkout: Checkout,
    /// The real repository's refs at snapshot time.
    pub pre_state: BTreeMap<String, String>,
    /// Creation time, seconds since the Unix epoch (see [`crate::now_unix`]).
    pub created_unix: u64,
    /// Fresh or kept.
    pub status: Status,
    /// How the rehearsed command ended, once it has run.
    ///
    /// Recorded so a rehearsal stays self-describing across processes: `git
    /// rehearse show <id>` tomorrow has to say "stopped on a conflict" without
    /// re-running anything, and re-deriving that from the sandbox's state
    /// would be guessing at what git did rather than remembering it.
    pub result: Option<Outcome>,
}

impl Meta {
    /// Writes `meta.json` into `root`, atomically.
    ///
    /// Written to a temporary file and renamed, so a crash mid-write leaves
    /// the previous `meta.json` intact rather than a truncated one — this file
    /// is the only record of the pre-state that apply verifies against.
    fn write(&self, root: &Path) -> Result<()> {
        let path = root.join(META_FILE);
        let tmp = root.join(META_TMP);
        let mut json =
            serde_json::to_string_pretty(self).map_err(|e| Error::Meta(path.clone(), e))?;
        json.push('\n');
        fs::write(&tmp, json).map_err(Error::io(&tmp))?;
        fs::rename(&tmp, &path).map_err(Error::io(&path))
    }

    /// Reads and validates the `meta.json` in `root`.
    fn read(root: &Path) -> Result<Self> {
        let path = root.join(META_FILE);
        let text = fs::read_to_string(&path).map_err(Error::io(&path))?;
        let meta: Self = serde_json::from_str(&text).map_err(|e| Error::Meta(path.clone(), e))?;
        if meta.schema != META_SCHEMA {
            return Err(Error::Sandbox(format!(
                "{}: rehearsal uses meta schema {}, this build understands {META_SCHEMA} — \
                 upgrade git-rehearse, or discard the rehearsal",
                path.display(),
                meta.schema
            )));
        }
        Ok(meta)
    }
}

/// A rehearsal directory on disk.
#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
    meta: Meta,
}

impl Sandbox {
    /// The rehearsal id, as used by `git rehearse show|apply|discard`.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.meta.id
    }

    /// The rehearsal directory: `meta.json`, the empty hooks directory, and
    /// the clone.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The shadow clone's worktree — where the rehearsed command runs.
    #[must_use]
    pub fn worktree(&self) -> PathBuf {
        self.root.join(WORKTREE_DIR)
    }

    /// Everything recorded about this rehearsal.
    #[must_use]
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Records how the rehearsed command ended.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::Meta`] if `meta.json` cannot be rewritten.
    pub fn record(&mut self, outcome: &Outcome) -> Result<()> {
        self.meta.result = Some(outcome.clone());
        self.meta.write(&self.root)
    }

    /// Marks the rehearsal as one to keep, so `list` shows it and the prune
    /// clock is the only thing that removes it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::Meta`] if `meta.json` cannot be rewritten.
    pub fn keep(&mut self) -> Result<()> {
        self.meta.status = Status::Kept;
        self.meta.write(&self.root)
    }

    /// Deletes the rehearsal, immediately and entirely.
    ///
    /// Safe by construction with respect to the real repository: the clone's
    /// object files are hardlinks, so removing them decrements a link count
    /// and never touches the real repo's copy.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the directory cannot be removed.
    pub fn discard(self) -> Result<()> {
        remove_rehearsal(&self.root)
    }
}

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
        status: Status::Fresh,
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
fn checkout(worktree: &Path, target: &Checkout) -> Result<()> {
    // The trailing `--` keeps a branch named like a file from being read as a
    // pathspec.
    match target {
        Checkout::Branch(name) => git::run(worktree, ["checkout", "--quiet", name, "--"])?,
        Checkout::Detached(sha) => {
            git::run(worktree, ["checkout", "--quiet", "--detach", sha, "--"])?
        }
    };
    Ok(())
}

/// Every rehearsal in the cache, newest last.
///
/// Pass `repo_id` to limit the listing to one repository — that is what
/// `git rehearse list` inside a repo wants.
///
/// Directories that do not parse as a rehearsal are skipped rather than
/// reported: a foreign directory or a half-deleted one must not break the
/// listing. [`prune`] is what eventually collects them.
///
/// # Errors
///
/// [`Error::Io`] if the cache root exists but cannot be read. A cache root
/// that does not exist yet is not an error — it is an empty list.
pub fn list(cache_root: &Path, repo_id: Option<&str>) -> Result<Vec<Sandbox>> {
    let mut found = Vec::new();
    for repo_dir in subdirectories(cache_root)? {
        if let Some(wanted) = repo_id
            && repo_dir.file_name().is_none_or(|name| name != wanted)
        {
            continue;
        }
        for root in subdirectories(&repo_dir)? {
            if let Ok(meta) = Meta::read(&root) {
                found.push(Sandbox { root, meta });
            }
        }
    }
    found.sort_by(|a, b| {
        a.meta
            .created_unix
            .cmp(&b.meta.created_unix)
            .then_with(|| a.meta.id.cmp(&b.meta.id))
    });
    Ok(found)
}

/// Finds one rehearsal of a repository.
///
/// `id` may be a full rehearsal id or a unique prefix of one — nobody wants to
/// type `1786248000-00` from a listing. Without an id, the most recent
/// rehearsal of that repository is the one meant, because "apply it" said
/// straight after a rehearsal can only mean that one.
///
/// # Errors
///
/// [`Error::Refused`] if there is no such rehearsal, or if a prefix matches
/// more than one — an ambiguous id must not be resolved by guessing.
pub fn find(cache_root: &Path, repo_id: &str, id: Option<&str>) -> Result<Sandbox> {
    let mut candidates = list(cache_root, Some(repo_id))?;
    let Some(id) = id else {
        return candidates.pop().ok_or_else(|| {
            Error::Refused(
                "no rehearsals for this repository.\n\
                 Rehearse something first: `git rehearse merge <branch>`."
                    .to_owned(),
            )
        });
    };

    let mut matches: Vec<Sandbox> = candidates
        .into_iter()
        .filter(|sandbox| sandbox.id() == id || sandbox.id().starts_with(id))
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(Error::Refused(format!(
            "no rehearsal {id} for this repository.\n\
             `git rehearse list` shows the ones there are."
        ))),
        _ => Err(Error::Refused(format!(
            "{id} matches {} rehearsals.\n\
             Use more of the id — `git rehearse list` shows them in full.",
            matches.len()
        ))),
    }
}

/// Deletes rehearsals older than `max_age_secs`, returning the ids removed.
///
/// Age comes from `meta.json` where there is one and from the directory's
/// modification time where there is not — a run killed mid-clone leaves a
/// directory with no `meta.json`, and nothing else would ever collect it.
///
/// # Errors
///
/// [`Error::Io`] if the cache root cannot be read or an expired rehearsal
/// cannot be removed.
pub fn prune(cache_root: &Path, now_unix: u64, max_age_secs: u64) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for repo_dir in subdirectories(cache_root)? {
        for root in subdirectories(&repo_dir)? {
            let created = Meta::read(&root)
                .map(|meta| meta.created_unix)
                .or_else(|_| directory_mtime(&root))?;
            if now_unix.saturating_sub(created) <= max_age_secs {
                continue;
            }
            remove_rehearsal(&root)?;
            if let Some(name) = root.file_name() {
                removed.push(name.to_string_lossy().into_owned());
            }
        }
        // A repository directory with nothing left in it is noise in the
        // cache; it comes back by itself on the next rehearsal.
        let _ = fs::remove_dir(&repo_dir);
    }
    removed.sort();
    Ok(removed)
}

/// Immediate, complete removal of one rehearsal directory.
fn remove_rehearsal(root: &Path) -> Result<()> {
    match fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        // Already gone is the outcome the caller wanted.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::Io(root.to_path_buf(), err)),
    }
}

/// The directories directly inside `dir`, sorted; empty if `dir` is absent.
fn subdirectories(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::Io(dir.to_path_buf(), err)),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::io(dir))?;
        if entry.file_type().map_err(Error::io(dir))?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// A directory's modification time, in seconds since the Unix epoch.
fn directory_mtime(dir: &Path) -> Result<u64> {
    let modified = fs::metadata(dir)
        .and_then(|meta| meta.modified())
        .map_err(Error::io(dir))?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::{Checkout, META_SCHEMA, Meta, Status};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sample() -> Meta {
        Meta {
            schema: META_SCHEMA,
            id: "1786248000-00".to_owned(),
            repo_id: "git-city-0123456789abcdef".to_owned(),
            repo_path: PathBuf::from("/repos/git-city"),
            command: vec!["rebase".to_owned(), "-i".to_owned(), "main".to_owned()],
            checkout: Checkout::Branch("feature".to_owned()),
            pre_state: BTreeMap::from([("refs/heads/main".to_owned(), "abc123".to_owned())]),
            created_unix: 1_786_248_000,
            status: Status::Fresh,
            result: None,
        }
    }

    #[test]
    fn meta_survives_a_round_trip_through_json() {
        let meta = sample();
        let json = serde_json::to_string_pretty(&meta).expect("serialises");
        let back: Meta = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(meta, back);
    }

    #[test]
    fn the_json_is_the_documented_shape() {
        let json = serde_json::to_string(&sample()).expect("serialises");
        // Apply reads pre_state out of this file; the field names are part of
        // the on-disk contract that META_SCHEMA versions.
        assert!(json.contains(r#""schema":1"#), "{json}");
        assert!(
            json.contains(r#""pre_state":{"refs/heads/main":"abc123"}"#),
            "{json}"
        );
        assert!(
            json.contains(r#""checkout":{"kind":"branch","target":"feature"}"#),
            "{json}"
        );
        assert!(json.contains(r#""status":"fresh""#), "{json}");
    }

    #[test]
    fn a_detached_checkout_round_trips_distinguishably() {
        let mut meta = sample();
        meta.checkout = Checkout::Detached("deadbeef".to_owned());
        let json = serde_json::to_string(&meta).expect("serialises");
        assert!(json.contains(r#""kind":"detached""#), "{json}");
        let back: Meta = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back.checkout, Checkout::Detached("deadbeef".to_owned()));
    }
}
