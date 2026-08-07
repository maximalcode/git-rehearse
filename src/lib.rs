//! git-rehearse — rehearse dangerous git commands in a shadow clone of your
//! real repository, inspect the outcome, then apply or discard.
//!
//! This library is the implementation; the `git-rehearse` binary is a thin
//! shell around it. [`SCOPE.md`] is the authoritative plan and the five design
//! principles in `CLAUDE.md` are binding on everything here — in particular
//! that the sandbox is **disposable and inert** (this module tree) and that
//! applying a rehearsal is a ref transplant, never a re-run.
//!
//! [`SCOPE.md`]: https://github.com/maximalcode/git-rehearse/blob/main/SCOPE.md

#![forbid(unsafe_code)]

pub mod analyze;
pub mod cache;
pub mod error;
pub mod execute;
pub mod git;
pub mod preflight;
pub mod report;
pub mod sandbox;

pub use error::{Error, Result};

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, as the timestamp everything in the cache is
/// stamped and pruned with.
///
/// Time is passed *into* [`sandbox::create`] and [`sandbox::prune`] rather
/// than read inside them, so both are testable without a clock: this is the
/// only place the wall clock is consulted.
///
/// A system clock before 1970 yields `0`, which makes the rehearsal look
/// ancient and prunable rather than failing the run outright.
#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
