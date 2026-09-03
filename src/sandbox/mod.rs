//! The shadow clone: creating one, describing one, finding one, throwing one
//! away.
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
//!
//! # How this module is laid out
//!
//! Three responsibilities that change for three different reasons, and one
//! type they share:
//!
//! - [`build`] — **lifecycle.** Clone, promote branches, strip remotes,
//!   disable hooks, carry config, check out. A sequence of git commands whose
//!   *order* is the contract.
//! - [`meta`] — **storage.** The `meta.json` schema and its atomic read and
//!   write. A versioned on-disk format with a compatibility obligation.
//! - [`store`] — **query.** Listing, finding by id or prefix, pruning by age.
//!   A directory tree being scanned.
//!
//! This file holds only what all three need: [`Sandbox`], [`Plan`], and the
//! two directory names. Everything public is re-exported here, so the split is
//! invisible from outside — `sandbox::create`, `sandbox::list`,
//! `sandbox::Meta` and the rest are spelled exactly as they were.

mod build;
mod merge_config;
mod meta;
mod store;

pub use build::create;
pub use meta::{Checkout, META_SCHEMA, Meta, Status};
pub use store::{DEFAULT_TTL_SECS, find, list, prune};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::carry::{Carry, Replay};
use crate::execute::Outcome;

/// The clone itself, inside the rehearsal directory.
const WORKTREE_DIR: &str = "sandbox";
/// The directory `core.hooksPath` points at, kept empty on purpose.
const HOOKS_DIR: &str = "no-hooks";

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
    /// The uncommitted work to carry through the rehearsal, if there was any.
    /// [`build`] moves it into the sandbox; [`crate::carry`] owns the rest.
    pub carry: Option<Carry>,
}

/// A rehearsal directory on disk.
///
/// The fields stay private to the module *tree*: [`build`] and [`store`] are
/// the only two places a `Sandbox` is ever constructed, and both are children
/// of this module.
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
    /// [`Error::Io`](crate::Error::Io) or [`Error::Meta`](crate::Error::Meta)
    /// if `meta.json` cannot be rewritten.
    pub fn record(&mut self, outcome: &Outcome) -> Result<()> {
        self.meta.result = Some(outcome.clone());
        self.meta.write(&self.root)
    }

    /// Records what became of the carried uncommitted work.
    ///
    /// Separate from [`Sandbox::record`] because the two are answered at
    /// different moments: the command's outcome the instant git exits, the
    /// replay's only afterwards — and, when the replay stopped and was
    /// resolved by hand, in an entirely later process.
    ///
    /// # Errors
    ///
    /// [`Error::Io`](crate::Error::Io) or [`Error::Meta`](crate::Error::Meta)
    /// if `meta.json` cannot be rewritten.
    pub fn record_replay(&mut self, replay: Replay) -> Result<()> {
        if let Some(carry) = self.meta.carry.as_mut() {
            carry.replay = Some(replay);
        }
        self.meta.write(&self.root)
    }

    /// Marks the rehearsal as one to keep, so `list` shows it and the prune
    /// clock is the only thing that removes it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`](crate::Error::Io) or [`Error::Meta`](crate::Error::Meta)
    /// if `meta.json` cannot be rewritten.
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
    /// [`Error::Io`](crate::Error::Io) if the directory cannot be removed.
    pub fn discard(self) -> Result<()> {
        store::remove_rehearsal(&self.root)
    }
}
