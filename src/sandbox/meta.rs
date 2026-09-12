//! What a rehearsal directory says about itself.
//!
//! A rehearsal is **self-describing**: everything a later invocation needs in
//! order to report on it or apply it lives in `meta.json` beside the clone, so
//! nothing about a rehearsal exists only in the memory of the process that
//! created it. `git rehearse apply <id>` tomorrow reads this file and needs no
//! other context.
//!
//! Separated from the lifecycle next door because it changes for a different
//! reason: this is an on-disk format with a version number and a compatibility
//! obligation, where [`super::build`] is a sequence of git commands.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::carry::Carry;
use crate::execute::Outcome;
use crate::{Error, Result};

/// Schema 3 adds durable worktree origin and makes older builds refuse it.
/// Version of the `meta.json` document. Bump on any incompatible change; a
/// build that meets an unfamiliar schema refuses the rehearsal rather than
/// half-reading it.
///
/// `2` added [`Meta::carry`] (#59). Deliberately a bump rather than a quiet
/// optional field: a rehearsal written by an older build carries no record of
/// the uncommitted work it did *not* carry, and applying it with a build that
/// now expects one would restore nothing while the report said otherwise.
pub const META_SCHEMA: u32 = 3;

const META_FILE: &str = "meta.json";
const META_TMP: &str = "meta.json.tmp";

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
    /// Kept on purpose, listed by `git rehearse list` until explicitly
    /// discarded.
    Kept,
}

/// The self-describing contents of `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// See [`META_SCHEMA`].
    pub schema: u32,
    /// Rehearsal id, unique within the repository's cache directory.
    pub id: String,
    /// Cache directory name for the repository, see [`crate::cache::repo_id`].
    pub repo_id: String,
    /// Where the real repository is.
    pub repo_path: PathBuf,
    /// Durable shared-repository and worktree administrative identity.
    #[serde(default)]
    pub origin: Option<crate::worktree::Origin>,
    /// The command being rehearsed.
    pub command: Vec<String>,
    /// What the sandbox has checked out.
    pub checkout: Checkout,
    /// The real repository's refs at snapshot time.
    pub pre_state: BTreeMap<String, String>,
    /// The uncommitted work this rehearsal carries, and what became of it.
    ///
    /// Self-describing for the same reason everything else here is: `apply`
    /// tomorrow has to know which changes were promised back, and prove that
    /// they are still the ones in the worktree, without the process that
    /// rehearsed them.
    pub carry: Option<Carry>,
    /// Creation time, seconds since the Unix epoch (see [`crate::now_unix`]).
    pub created_unix: u64,
    /// Fresh or explicitly kept. Kept metadata is durable and is not age
    /// pruned; this makes `--keep` a reliable handoff across restarts.
    pub status: Status,
    /// How the rehearsed command ended, once it has run.
    ///
    /// Recorded so a rehearsal stays self-describing across processes: `git
    /// rehearse show <id>` tomorrow has to say "stopped on a conflict" without
    /// re-running anything, and re-deriving that from the sandbox's state
    /// would be guessing at what git did rather than remembering it.
    pub result: Option<Outcome>,
    /// Optional extensions survive migration and later metadata updates.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl Meta {
    /// Writes `meta.json` into `root`, atomically.
    ///
    /// Written to a temporary file and renamed, so a crash mid-write leaves
    /// the previous `meta.json` intact rather than a truncated one — this file
    /// is the only record of the pre-state that apply verifies against.
    pub(super) fn write(&self, root: &Path) -> Result<()> {
        let path = root.join(META_FILE);
        let tmp = root.join(META_TMP);
        let mut json =
            serde_json::to_string_pretty(self).map_err(|e| Error::Meta(path.clone(), e))?;
        json.push('\n');
        let mut file = fs::File::create(&tmp).map_err(Error::io(&tmp))?;
        file.write_all(json.as_bytes()).map_err(Error::io(&tmp))?;
        file.sync_all().map_err(Error::io(&tmp))?;
        fs::rename(&tmp, &path).map_err(Error::io(&path))
    }

    /// Reads and validates the `meta.json` in `root`.
    pub(super) fn read(root: &Path) -> Result<Self> {
        let path = root.join(META_FILE);
        let text = fs::read_to_string(&path).map_err(Error::io(&path))?;
        let mut document: Value =
            serde_json::from_str(&text).map_err(|e| Error::Meta(path.clone(), e))?;
        let schema = document
            .get("schema")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Error::Sandbox(format!(
                    "{}: rehearsal metadata has no numeric schema",
                    path.display()
                ))
            })?;

        if schema == 1 {
            // Schema 1 predates dirty-worktree carrying. Its fields retain
            // their meanings, so the only safe migration is to add the
            // explicit empty carry record and write the current schema without
            // inventing an origin. Such a preview remains reference material.
            let object = document.as_object_mut().ok_or_else(|| {
                Error::Sandbox(format!(
                    "{}: rehearsal metadata is not an object",
                    path.display()
                ))
            })?;
            object.insert("schema".to_owned(), Value::from(META_SCHEMA));
            object.insert("carry".to_owned(), Value::Null);
            let meta: Self =
                serde_json::from_value(document).map_err(|e| Error::Meta(path.clone(), e))?;
            meta.write(root)?;
            return Ok(meta);
        }

        if schema == 2 {
            // Preserve older previews as reference material; their missing origin
            // cannot authorize Apply, and the original file is not rewritten.
            return serde_json::from_value(document).map_err(|e| Error::Meta(path, e));
        }

        if schema != u64::from(META_SCHEMA) {
            return Err(Error::Sandbox(format!(
                "{}: rehearsal uses meta schema {schema}, this build understands {META_SCHEMA} — \
                 upgrade git-rehearse, or discard the rehearsal",
                path.display()
            )));
        }

        serde_json::from_value(document).map_err(|e| Error::Meta(path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::{Checkout, META_SCHEMA, Meta, Status};
    use crate::carry::{Carry, Replay};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sample() -> Meta {
        Meta {
            schema: META_SCHEMA,
            id: "1786248000-00".to_owned(),
            repo_id: "git-city-0123456789abcdef".to_owned(),
            repo_path: PathBuf::from("/repos/git-city"),
            origin: None,
            command: vec!["rebase".to_owned(), "-i".to_owned(), "main".to_owned()],
            checkout: Checkout::Branch("feature".to_owned()),
            pre_state: BTreeMap::from([("refs/heads/main".to_owned(), "abc123".to_owned())]),
            carry: None,
            created_unix: 1_786_248_000,
            status: Status::Fresh,
            result: None,
            extensions: BTreeMap::new(),
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
        assert!(json.contains(r#""schema":3"#), "{json}");
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
    fn the_carried_work_and_its_replay_survive_the_file() {
        // `apply` in a later process reads both out of here: which changes
        // were promised back, and the commit that holds the rehearsed result.
        let mut meta = sample();
        meta.carry = Some(Carry {
            snapshot: "5ea51a5h".to_owned(),
            paths: vec!["src/main.rs".to_owned()],
            replay: Some(Replay::Restored {
                result: Some("re914yed".to_owned()),
            }),
        });
        let json = serde_json::to_string(&meta).expect("serialises");
        assert!(json.contains(r#""kind":"restored""#), "{json}");
        let back: Meta = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back.carry, meta.carry);
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
