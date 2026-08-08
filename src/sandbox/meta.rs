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
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::execute::Outcome;
use crate::{Error, Result};

/// Version of the `meta.json` document. Bump on any incompatible change; a
/// build that meets an unfamiliar schema refuses the rehearsal rather than
/// half-reading it.
pub const META_SCHEMA: u32 = 1;

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
    /// Kept on purpose, listed by `git rehearse list` until it ages out.
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
    pub(super) fn write(&self, root: &Path) -> Result<()> {
        let path = root.join(META_FILE);
        let tmp = root.join(META_TMP);
        let mut json =
            serde_json::to_string_pretty(self).map_err(|e| Error::Meta(path.clone(), e))?;
        json.push('\n');
        fs::write(&tmp, json).map_err(Error::io(&tmp))?;
        fs::rename(&tmp, &path).map_err(Error::io(&path))
    }

    /// Reads and validates the `meta.json` in `root`.
    pub(super) fn read(root: &Path) -> Result<Self> {
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
