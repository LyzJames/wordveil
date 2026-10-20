//! Crash-safe file writes (design 12.2, 12.8). Data reaches the device before
//! a write-through rename, so a power loss leaves either the previous or the
//! new complete file at the target path, never a truncated one.
use crate::{Error, Result};
use std::{io::Write, path::Path};

/// Flush a file's data and metadata to the device.
pub fn sync_file(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

/// Move `from` over `to` on the same volume and return once the move is on
/// disk (MOVEFILE_WRITE_THROUGH). The source must already be flushed.
pub fn replace(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let wide = |path: &Path| path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let (source, target) = (wide(from), wide(to));
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)?;
        if let Some(parent) = to.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

/// Replace `target` with `bytes` through a flushed sibling temporary file.
pub fn write(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target.parent().ok_or(Error::Integrity)?;
    let name = target.file_name().ok_or(Error::Integrity)?.to_string_lossy().to_string();
    let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let outcome = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, target)
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    outcome
}
