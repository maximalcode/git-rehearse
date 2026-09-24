//! Process ownership on stable files outside removable rehearsal directories.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::{Error, Result};

/// Released by the operating system even when its owning process crashes.
#[derive(Debug)]
pub(super) struct Ownership(File);

impl Ownership {
    /// Never unlink these files: a waiting process may still hold the old inode.
    pub(super) fn acquire(root: &Path) -> Result<Self> {
        let path = root.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(Error::io(&path))?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(TryLockError::WouldBlock) => Err(Error::Refused(format!(
                "rehearsal {} is active in another process; wait for execution to finish",
                root.file_name().unwrap_or_default().to_string_lossy()
            ))),
            Err(TryLockError::Error(error)) => Err(Error::Io(path, error)),
        }
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
