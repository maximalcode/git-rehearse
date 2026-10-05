//! Copy mutable conflict-resolution data, never link it to the original cache.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::{Error, Result};

pub(super) fn copy(repo: &Path, worktree: &Path) -> Result<()> {
    let origin = crate::worktree::Origin::capture(repo)?;
    let source = origin.common_dir.join("rr-cache");
    match fs::symlink_metadata(&source) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::Io(source, error)),
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(Error::Sandbox("rerere cache must be a directory".into())),
    }
    copy_entry(&source, &worktree.join(".git/rr-cache"))
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(Error::io(source))?;
    if metadata.is_dir() {
        fs::create_dir(destination).map_err(Error::io(destination))?;
        for entry in fs::read_dir(source).map_err(Error::io(source))? {
            let entry = entry.map_err(Error::io(source))?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        fs::copy(source, destination).map_err(Error::io(source))?;
    } else {
        return Err(Error::Sandbox(format!(
            "cannot copy rerere cache entry {}: expected a regular file or directory",
            source.display()
        )));
    }
    Ok(())
}
