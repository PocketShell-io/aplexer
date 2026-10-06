//! Crash-safe persistence primitives shared by every on-disk artifact:
//! temp-file-and-rename writes for JSON records and raw bytes, and advisory
//! whole-file locks.
//!
//! Unix publishes with `rename`/`renameat2(RENAME_EXCHANGE)` and fsyncs the
//! parent directory. Windows publishes with `MoveFileExW(REPLACE_EXISTING |
//! WRITE_THROUGH)` / `ReplaceFileW`, flushes the file with `FlushFileBuffers`
//! (`sync_all`), has no directory fsync, and locks with `LockFileEx`.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::{ensure_private_dir, persist_worker_identity_once};

pub(crate) static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct AtomicTempGuard(PathBuf);

impl Drop for AtomicTempGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Create a brand-new file that only the owner can read. Unix: mode 0600.
/// Windows: the parent directory's protected owner-only DACL is inherited.
fn create_new_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

/// Atomically move `from` over `to`.
fn publish_rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        crate::sys::windows::fs::replace_file(from, to)
    }
}

/// Make a directory entry change durable. A no-op on Windows, where NTFS
/// metadata is journaled and directories cannot be opened for flushing.
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    ensure_private_dir(parent)?;
    let value = serde_json::to_value(value)?;
    persist_worker_identity_once(path, &value)?;
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .unwrap_or(OsStr::new("record"))
            .to_string_lossy(),
        std::process::id(),
        seq
    ));
    let mut file =
        create_new_private(&temp).with_context(|| format!("create {}", temp.display()))?;
    let _temp_guard = AtomicTempGuard(temp.clone());
    serde_json::to_writer_pretty(&mut file, &value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    publish_rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    sync_dir(parent)?;
    Ok(())
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = parent_dir(path)?;
    ensure_private_dir(parent)?;
    write_atomically(parent, path, bytes, 0o600, publish_rename)
}

/// `atomic_write_bytes` with an explicit file mode, for files outside
/// aplexer's private state tree (engine configs in the user's home). The
/// parent directory must already exist and is left exactly as found: this
/// never forces it private. On Windows the mode is ignored; the file takes
/// its parent's ACL.
pub fn atomic_write_bytes_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    write_atomically(parent_dir(path)?, path, bytes, mode, publish_rename)
}

/// History checkpoints may create files, but never revive a deleted session directory.
pub(crate) fn atomic_write_json_in_existing_dir<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write_bytes_with_mode(path, &bytes, 0o600)
}

/// Running workers may replace a record, never create one. The publication
/// step checks existence (Unix: `RENAME_EXCHANGE`; Windows: `ReplaceFileW`),
/// so deletion after serialization still wins. Startup uses
/// `atomic_write_json` instead. Unsupported filesystems fail the write
/// rather than use a racy fallback.
pub(crate) fn replace_existing_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        bail!("record is not a regular file: {}", path.display());
    }
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_atomically(parent_dir(path)?, path, &bytes, 0o600, exchange_existing)
}

#[cfg(unix)]
fn exchange_existing(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(feature = "startup-test-hooks")]
    if std::env::var_os("APLEXER_TEST_FAIL_RECORD_EXCHANGE").is_some() {
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn exchange_existing(from: &Path, to: &Path) -> io::Result<()> {
    crate::sys::windows::fs::replace_existing_file(from, to)
}

fn parent_dir(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))
}

#[cfg_attr(windows, allow(unused_variables))]
fn write_atomically(
    parent: &Path,
    path: &Path,
    bytes: &[u8],
    mode: u32,
    publish: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .unwrap_or(OsStr::new("bytes"))
            .to_string_lossy(),
        std::process::id(),
        seq
    ));
    #[cfg_attr(windows, allow(unused_mut))]
    let mut file =
        create_new_private(&temp).with_context(|| format!("create {}", temp.display()))?;
    let _temp_guard = AtomicTempGuard(temp.clone());
    // Created private, then widened while still empty: fchmod is not
    // subject to the umask, so the requested mode lands exactly, and no
    // content is ever visible at a wider mode than it will end up with.
    #[cfg(unix)]
    if mode != 0o600 {
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {}", temp.display()))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    // Windows cannot rename over a file with an open handle that lacks
    // FILE_SHARE_DELETE, and our own handle must not be the reason.
    drop(file);
    publish(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    sync_dir(parent)?;
    Ok(())
}

/// Reads a small file the caller has already opened and vetted, refusing
/// more than `cap` bytes both by size up front and by a recount after the
/// read, so a regular file that grows after fstat is rejected rather than
/// parsed from a truncated prefix.
pub(crate) fn read_bounded(file: File, path: &Path, label: &str, cap: usize) -> Result<Vec<u8>> {
    let length = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?
        .len();
    if length > cap as u64 {
        bail!(
            "{label} {} exceeds the {cap}-byte cap (got {length} bytes)",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label} {}", path.display()))?;
    if bytes.len() > cap {
        bail!("{label} {} exceeds the {cap}-byte cap", path.display());
    }
    Ok(bytes)
}

/// `read_bounded`, parsed as JSON.
pub(crate) fn read_bounded_json<T: DeserializeOwned>(
    file: File,
    path: &Path,
    label: &str,
    cap: usize,
) -> Result<T> {
    let bytes = read_bounded(file, path, label, cap)?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {label} {}", path.display()))
}

/// Opens an existing file read-only without following a final-component
/// symlink (Windows: any reparse point is refused) or blocking on an
/// accidental FIFO/device. The caller still inspects the file type.
pub(crate) fn open_no_follow_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
        options.open(path)
    }
    #[cfg(windows)]
    {
        crate::sys::windows::fs::open_no_follow(path, &mut options)
    }
}

/// Opens `path` without following a final-component symlink or blocking
/// on an accidental FIFO/device, then reads it whole under `cap`. `None`
/// when absent, so callers with a documented empty state keep it; every
/// other file type and any oversize fails closed.
pub(crate) fn read_bounded_regular_file(
    path: &Path,
    label: &str,
    cap: usize,
) -> Result<Option<Vec<u8>>> {
    let file = match open_no_follow_read(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open {label} {}", path.display()))
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    Ok(Some(read_bounded(file, path, label, cap)?))
}

/// Whole-file advisory lock (Unix `flock`, Windows `LockFileEx`), released
/// on drop or when the process dies.
pub struct FileLock {
    file: File,
}
impl FileLock {
    pub fn exclusive(path: &Path, nonblocking: bool) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| anyhow!("lock has no parent"))?;
        ensure_private_dir(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            let mut op = libc::LOCK_EX;
            if nonblocking {
                op |= libc::LOCK_NB;
            }
            if unsafe { libc::flock(file.as_raw_fd(), op) } != 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("lock {}", path.display()));
            }
        }
        #[cfg(windows)]
        crate::sys::windows::fs::lock_exclusive(&file, nonblocking)
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(Self { file })
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        #[cfg(windows)]
        crate::sys::windows::fs::unlock(&self.file);
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;

    #[test]
    fn deletion_between_staging_and_publication_wins() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("session.json");
        fs::write(&record, b"old").unwrap();
        let error = write_atomically(dir.path(), &record, b"new", 0o600, |temp, path| {
            fs::remove_file(path)?;
            exchange_existing(temp, path)
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn replacement_is_atomic_and_cleans_up_displaced_record() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("session.json");
        fs::write(&record, b"old").unwrap();
        replace_existing_json(&record, &"new").unwrap();
        assert_eq!(fs::read_to_string(&record).unwrap(), "\"new\"\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_bytes_round_trip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sub").join("f.bin");
        atomic_write_bytes(&target, b"one").unwrap();
        atomic_write_bytes(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert_eq!(
            read_bounded_regular_file(&target, "t", 16).unwrap().unwrap(),
            b"two"
        );
        assert!(read_bounded_regular_file(&target, "t", 2).is_err());
        assert!(read_bounded_regular_file(&dir.path().join("none"), "t", 2)
            .unwrap()
            .is_none());
        // Only the target remains; no staged temp files leak.
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn nonblocking_lock_contends_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("x.lock");
        let held = FileLock::exclusive(&path, true).unwrap();
        let error = FileLock::exclusive(&path, true).err().unwrap();
        assert_eq!(
            crate::io_kind(&error),
            Some(io::ErrorKind::WouldBlock),
            "{error:#}"
        );
        drop(held);
        FileLock::exclusive(&path, true).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn reparse_point_records_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        fs::write(&real, b"{}").unwrap();
        let link = dir.path().join("link.json");
        if std::os::windows::fs::symlink_file(&real, &link).is_err() {
            return; // symlink privilege unavailable
        }
        assert!(read_bounded_regular_file(&link, "t", 16).is_err());
    }
}
