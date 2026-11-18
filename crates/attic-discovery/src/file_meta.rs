//! File metadata used by the freshness fast path.
//!
//! A file is treated as unchanged WITHOUT reading its bytes only when size,
//! last-write time, change time and file identity all equal the values
//! captured at index time (and the write time is old enough to trust — see
//! the racy-file rule in `attic-incremental`). Change time and file identity
//! matter because copy tools (Explorer, robocopy, `cp -p`, unzip, `rsync -t`)
//! preserve the source's last-write time: in-place rewrites still move
//! NTFS ChangeTime / Unix ctime, and replacements get a new file identity.
//!
//! On Windows, Rust std exposes neither ChangeTime nor the 128-bit file id
//! on stable, so they are read with `GetFileInformationByHandleEx`.

use std::path::Path;
use std::time::UNIX_EPOCH;

/// Metadata snapshot of one filesystem object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Last-write time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// NTFS ChangeTime / Unix ctime, nanoseconds since the Unix epoch.
    /// `None` when the platform cannot report it.
    pub change_time_ns: Option<i64>,
    /// Stable identity: `volume:fileid` (Windows) or `dev:ino` (Unix).
    /// `None` when the platform cannot report it.
    pub file_id: Option<String>,
}

/// Read metadata for a file or directory without following a final symlink.
pub fn file_meta(path: &Path) -> std::io::Result<FileMeta> {
    let md = std::fs::symlink_metadata(path)?;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let size = if md.is_dir() { 0 } else { md.len() };
    let (change_time_ns, file_id) = platform::change_time_and_id(path, &md);
    Ok(FileMeta {
        size,
        mtime_ns,
        change_time_ns,
        file_id,
    })
}

/// Identity of a repository root directory (`volume:fileid` / `dev:ino`).
/// A different value for the same path means the directory was replaced
/// (re-cloned, remounted elsewhere); `None` when it does not exist.
pub fn root_identity(path: &Path) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_dir() {
        return None;
    }
    platform::change_time_and_id(path, &md).1
}

#[cfg(unix)]
mod platform {
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub(super) fn change_time_and_id(
        _path: &Path,
        md: &std::fs::Metadata,
    ) -> (Option<i64>, Option<String>) {
        let ctime = md.ctime().saturating_mul(1_000_000_000) + md.ctime_nsec();
        (Some(ctime), Some(format!("{}:{}", md.dev(), md.ino())))
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo,
        FileIdInfo, GetFileInformationByHandleEx,
    };

    /// 100 ns ticks between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF_TICKS: i64 = 116_444_736_000_000_000;

    pub(super) fn change_time_and_id(
        path: &Path,
        _md: &std::fs::Metadata,
    ) -> (Option<i64>, Option<String>) {
        let Ok(file) = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
        else {
            return (None, None);
        };
        let handle = file.as_raw_handle() as HANDLE;
        (change_time(handle), file_id(handle))
    }

    fn change_time(handle: HANDLE) -> Option<i64> {
        // SAFETY: `handle` is a live file handle owned by the caller's
        // `File`; the buffer is a properly aligned, correctly sized
        // FILE_BASIC_INFO written only by the call.
        let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&mut info as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        Some((info.ChangeTime - EPOCH_DIFF_TICKS).saturating_mul(100))
    }

    fn file_id(handle: HANDLE) -> Option<String> {
        // SAFETY: as above, for FILE_ID_INFO.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let id: String = info
            .FileId
            .Identifier
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Some(format!("{:x}:{id}", info.VolumeSerialNumber))
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::path::Path;
    pub(super) fn change_time_and_id(
        _path: &Path,
        _md: &std::fs::Metadata,
    ) -> (Option<i64>, Option<String>) {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_place_rewrite_with_restored_mtime_moves_change_time() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"aaaa").unwrap();
        let before = file_meta(&p).unwrap();
        assert_eq!(before.size, 4);
        assert!(before.file_id.is_some());
        assert!(before.change_time_ns.is_some());

        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&p, b"bbbb").unwrap();
        // Restore the original last-write time, as copy tools do.
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(UNIX_EPOCH + std::time::Duration::from_nanos(before.mtime_ns as u64))
            .unwrap();
        drop(f);

        let after = file_meta(&p).unwrap();
        assert_eq!(after.size, before.size);
        assert_eq!(after.mtime_ns, before.mtime_ns);
        assert_ne!(after.change_time_ns, before.change_time_ns);
    }

    #[test]
    fn replaced_file_gets_new_identity_and_root_identity_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"x").unwrap();
        let a = file_meta(&p).unwrap();
        let tmp = dir.path().join("b.txt");
        std::fs::write(&tmp, b"x").unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        let b = file_meta(&p).unwrap();
        assert_ne!(a.file_id, b.file_id);

        let r1 = root_identity(dir.path()).unwrap();
        assert_eq!(Some(r1), root_identity(dir.path()));
        assert_eq!(root_identity(&dir.path().join("missing")), None);
    }
}
