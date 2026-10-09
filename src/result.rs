//! The public identity of a retained rehearsal result.
//!
//! A rehearsal id identifies a cache entry.  It does not identify the exact
//! result a client reviewed: a stopped rehearsal can be continued and its
//! carried work can be replayed again.  This module derives the versioned
//! result revision and the frozen endpoints that make that distinction
//! explicit.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::carry::Carry;
use crate::sandbox::{Checkout, Sandbox};
use crate::{Error, Result, cache, git};

/// The version prefix for the public reviewed-result revision.
pub const REVISION_PREFIX: &str = "rr1:";

/// The frozen endpoints of a completed result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EndpointDescriptors {
    /// Endpoint descriptor format version, independent of the report schema.
    pub version: u32,
    /// The exact rehearsal and originating worktree identity.
    pub origin: OriginDescriptor,
    pub action: Vec<String>,
    pub checkout: Checkout,
    /// The refs and HEAD from which the result was rehearsed.
    pub pre_state: BTreeMap<String, String>,
    /// The corresponding refs and HEAD in the sandbox after the command.
    pub results: BTreeMap<String, String>,
    /// The carried object endpoints, when this rehearsal carried work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carried: Option<CarriedEndpoints>,
}

/// The durable identity of the originating repository/worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OriginDescriptor {
    pub rehearsal: String,
    pub repository: String,
    pub repository_id: String,
    pub common_dir: String,
    pub git_dir: String,
}

/// Frozen object endpoints for carried work.  The ref targets are included in
/// addition to metadata values so a changed carried object is a changed
/// result even if a high-level carry record was left untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CarriedEndpoints {
    pub snapshot: String,
    pub snapshot_ref: Option<String>,
    /// Whether the snapshot ref was resolved. `Missing` is deliberately
    /// distinct from an absent field in an older endpoint description.
    pub snapshot_ref_status: CarriedEndpointStatus,
    pub replay: Option<String>,
    pub replay_ref: Option<String>,
    /// `NotProduced` and `NotNeeded` are valid clean outcomes; `Missing`
    /// means metadata promised a replay object but its ref is gone; and
    /// `Unexpected` means a ref exists where metadata says no replay object
    /// was produced.
    pub replay_ref_status: CarriedEndpointStatus,
    pub paths: Vec<String>,
}

/// Availability of the immutable refs used to carry tracked work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarriedEndpointStatus {
    Present,
    Missing,
    NotProduced,
    NotNeeded,
    Unexpected,
}

/// A calculated result, retained for Apply so the compared candidate is the
/// same candidate that is transplanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub revision: String,
    pub endpoints: EndpointDescriptors,
}

impl Candidate {
    /// The refs in the sandbox after the rehearsed command.
    #[must_use]
    pub fn results(&self) -> &BTreeMap<String, String> {
        &self.endpoints.results
    }
}

/// Calculates the result while the caller owns the rehearsal.
///
/// # Errors
///
/// Returns a refusal when no durable outcome exists or when a required
/// endpoint cannot be inspected.
pub fn calculate(sandbox: &Sandbox) -> Result<Candidate> {
    let meta = sandbox.meta();
    match meta.result.as_ref() {
        Some(crate::execute::Outcome::Clean) => {}
        Some(crate::execute::Outcome::Stopped { .. }) => {
            return Err(Error::Refused(format!(
                "rehearsal {} stopped before it had a completed result to review or apply",
                meta.id
            )));
        }
        Some(crate::execute::Outcome::Failed { .. }) => {
            return Err(Error::Refused(format!(
                "rehearsal {} failed before it had a completed result to review or apply",
                meta.id
            )));
        }
        None => {
            return Err(Error::Refused(format!(
                "rehearsal {} is still incomplete and has no completed result to review or apply",
                meta.id
            )));
        }
    }
    let results = state(&sandbox.worktree())?;
    let carried = carried_endpoints(&sandbox.worktree(), meta.carry.as_ref())?;
    let origin = meta.origin.as_ref().ok_or_else(|| {
        Error::Refused(
            "the rehearsal has no durable origin; keep it for reference and rehearse again"
                .to_owned(),
        )
    })?;
    let endpoints = EndpointDescriptors {
        version: 1,
        origin: OriginDescriptor {
            rehearsal: meta.id.clone(),
            repository: meta.repo_path.display().to_string(),
            repository_id: cache::repo_id(&origin.common_dir),
            common_dir: origin.common_dir.display().to_string(),
            git_dir: origin.git_dir.display().to_string(),
        },
        action: meta.command.clone(),
        checkout: meta.checkout.clone(),
        pre_state: meta.pre_state.clone(),
        results,
        carried,
    };
    let canonical = Canonical {
        endpoints: &endpoints,
        outcome: meta.result.as_ref().expect("checked above"),
    };
    let bytes = serde_json::to_string(&canonical)
        .map_err(|error| Error::Sandbox(format!("could not encode result revision: {error}")))?;
    let digest = git::run_with_stdin(
        &sandbox.worktree(),
        ["hash-object", "--stdin"],
        Some(&bytes),
    )?;
    let digest = digest.trim();
    if digest.is_empty() {
        return Err(Error::Sandbox(
            "git returned an empty result revision".to_owned(),
        ));
    }
    Ok(Candidate {
        revision: format!("{REVISION_PREFIX}{digest}"),
        endpoints,
    })
}

/// Validates the public revision spelling before a conditional Apply can do
/// anything with the retained rehearsal.
pub fn validate_revision(revision: &str) -> Result<()> {
    let digest = revision.strip_prefix(REVISION_PREFIX).ok_or_else(|| {
        Error::Refused(
            "malformed result revision; refresh the rehearsal review and use its rr1: revision"
                .to_owned(),
        )
    })?;
    if !matches!(digest.len(), 40 | 64) || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Refused(
            "malformed result revision; refresh the rehearsal review and use its rr1: revision"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Captures the branch refs and `HEAD` used to compare a repository state.
pub(crate) fn state(worktree: &std::path::Path) -> Result<BTreeMap<String, String>> {
    let mut refs = git::refs(worktree, "refs/heads/", 0)?;
    if let Ok(head) = git::run(worktree, ["rev-parse", "--verify", "--quiet", "HEAD"]) {
        refs.insert("HEAD".to_owned(), head);
    }
    Ok(refs)
}

fn carried_endpoints(
    worktree: &std::path::Path,
    carry: Option<&Carry>,
) -> Result<Option<CarriedEndpoints>> {
    let Some(carry) = carry else { return Ok(None) };
    // A missing ref is data in the public descriptor, not an error to hide:
    // the status fields distinguish a missing object from a valid contained or
    // not-needed replay. Other Git errors still abort descriptor calculation.
    let snapshot_ref = resolve_ref(worktree, "refs/rehearse/carried")?;
    let replay_ref = resolve_ref(worktree, "refs/rehearse/replayed")?;
    let snapshot_ref = snapshot_ref.ok_or_else(|| {
        Error::Refused(
            "the carried snapshot endpoint is missing; refresh the rehearsal review before applying"
                .to_owned(),
        )
    })?;
    let (replay, replay_ref_status) = match carry.replay.as_ref() {
        Some(crate::carry::Replay::Restored {
            result: Some(result),
        }) => (
            Some(result.clone()),
            if replay_ref.is_some() {
                CarriedEndpointStatus::Present
            } else {
                return Err(Error::Refused(
                    "the carried replay endpoint is missing; refresh the rehearsal review before applying"
                        .to_owned(),
                ));
            },
        ),
        Some(crate::carry::Replay::Restored { result: None }) => (
            None,
            if replay_ref.is_some() {
                return Err(Error::Refused(
                    "the carried replay endpoint exists without a recorded replay result; refresh the rehearsal review"
                        .to_owned(),
                ));
            } else {
                CarriedEndpointStatus::NotProduced
            },
        ),
        Some(crate::carry::Replay::NotNeeded) => (
            None,
            if replay_ref.is_some() {
                return Err(Error::Refused(
                    "the carried replay endpoint exists for a not-needed replay; refresh the rehearsal review"
                        .to_owned(),
                ));
            } else {
                CarriedEndpointStatus::NotNeeded
            },
        ),
        Some(crate::carry::Replay::Conflicted { .. } | crate::carry::Replay::Refused { .. })
        | None => (None, CarriedEndpointStatus::Missing),
    };
    let snapshot_ref_status = CarriedEndpointStatus::Present;
    Ok(Some(CarriedEndpoints {
        snapshot: carry.snapshot.clone(),
        snapshot_ref: Some(snapshot_ref),
        snapshot_ref_status,
        replay,
        replay_ref,
        replay_ref_status,
        paths: carry.paths.clone(),
    }))
}

fn resolve_ref(worktree: &std::path::Path, name: &str) -> Result<Option<String>> {
    let commit_ref = format!("{name}^{{commit}}");
    match git::run(worktree, ["rev-parse", "--verify", "--quiet", &commit_ref]) {
        Ok(value) => Ok(Some(value)),
        // `--quiet` reports a genuinely absent ref as exit 1 with no stderr.
        // A malformed ref, unreadable object database, or wrong object type
        // must remain an error so the public report can explain why its
        // authoritative endpoints are unavailable.
        Err(Error::Git {
            code: Some(1),
            stderr,
            ..
        }) if stderr.is_empty() => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Serialize)]
struct Canonical<'a> {
    endpoints: &'a EndpointDescriptors,
    outcome: &'a crate::execute::Outcome,
}

#[cfg(test)]
mod tests {
    use super::{REVISION_PREFIX, validate_revision};

    #[test]
    fn revisions_are_versioned_git_object_ids() {
        assert!(validate_revision("rr1:0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(validate_revision("rr1:not-a-digest").is_err());
        assert!(validate_revision("rr2:0123456789abcdef0123456789abcdef01234567").is_err());
        assert_eq!(REVISION_PREFIX, "rr1:");
    }
}
