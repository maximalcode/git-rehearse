//! Signature presence on the actual result objects. No verifier is invoked:
//! an embedded signature is not evidence of validity or trust.
use std::collections::BTreeSet;
use std::path::Path;

use crate::analyze::RefMove;
use crate::{Result, git};

/// One inspected commit object, deduplicated across changed refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub sha: String,
    pub present: bool,
}

pub(crate) fn inspect(worktree: &Path, moves: &[RefMove]) -> Result<Vec<Signature>> {
    let mut commits = BTreeSet::new();
    for moved in moves {
        let Some(after) = &moved.after else { continue };
        // Include the resulting tip even for a backward ref move. The range
        // also covers every intermediate replay and fast-forwarded commit.
        commits.insert(after.clone());
        let mut args = vec!["rev-list".to_owned(), after.clone()];
        if let Some(before) = &moved.before {
            args.push(format!("^{before}"));
        }
        args.push("--".to_owned());
        commits.extend(git::run(worktree, args)?.lines().map(str::to_owned));
    }
    commits
        .into_iter()
        .map(|sha| {
            let object = git::run_bytes(worktree, ["cat-file", "commit", &sha])?;
            Ok(Signature {
                sha,
                present: has_signature(&object),
            })
        })
        .collect()
}

fn has_signature(object: &[u8]) -> bool {
    object
        .split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty())
        .any(|line| line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 "))
}

#[cfg(test)]
mod tests {
    use super::has_signature;

    #[test]
    fn only_commit_headers_establish_presence() {
        assert!(has_signature(
            b"tree abc\ngpgsig signature\n continuation\n\nmessage"
        ));
        assert!(has_signature(
            b"tree abc\ngpgsig-sha256 signature\n\nmessage"
        ));
        assert!(!has_signature(b"tree abc\n\ngpgsig message text"));
        assert!(!has_signature(
            b"tree abc\nmergetag tag\n gpgsig embedded tag\n\nmessage"
        ));
    }
}
