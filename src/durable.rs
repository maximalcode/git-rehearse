//! Filesystem durability shared by metadata and recovery publication.

#[cfg(not(windows))]
use std::fs::File;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::path::Path;

use crate::{Error, Result};

/// Flushes the directory entry created by a publication or removed by cleanup.
///
/// Windows requires `FILE_FLAG_BACKUP_SEMANTICS` to obtain a directory handle,
/// and `FlushFileBuffers` requires `GENERIC_WRITE`; `File::open` supplies
/// neither. Rust's `File::sync_all` calls `FlushFileBuffers` on that handle, so
/// keep the error visible instead of treating an unflushed directory as safe.
pub(crate) fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // FILE_FLAG_BACKUP_SEMANTICS from WinBase.h. CreateFileW requires it
        // for directory handles; see the CreateFileW directory contract.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(Error::io(parent))?;
    }

    #[cfg(not(windows))]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(Error::io(parent))?;
    }

    Ok(())
}
