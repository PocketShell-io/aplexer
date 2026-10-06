//! Atomic replace, file locks (LockFileEx), private dirs/ACLs, reparse-point
//! checks, home/state/runtime dirs. Owner: agent "fs-paths".
//!
//! Seam API (all `pub`, called from `#[cfg(windows)]` branches):
//! - [`home_dir`], [`local_app_data`], [`app_data`]: `USERPROFILE` / `%LOCALAPPDATA%` / `%APPDATA%`.
//! - [`lock_exclusive`] / [`unlock`]: `LockFileEx` byte-range lock on a high,
//!   content-free offset. Kernel releases it on handle close or process death.
//!   A contended non-blocking attempt maps to `io::ErrorKind::WouldBlock`
//!   (what `flock(LOCK_NB)` produced on Unix).
//! - [`replace_file`]: `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`, creating
//!   the target if absent. [`replace_existing_file`]: `ReplaceFileW`, which
//!   fails `NotFound` when the target is gone (the `RENAME_EXCHANGE` contract).
//!   Both retry briefly on transient sharing violations.
//! - [`harden_dir_handle`] / [`ensure_private_dir`]: create a directory chain
//!   refusing reparse points, then give the leaf an owner-only protected DACL.
//! - [`open_no_follow`]: open a file with `FILE_FLAG_OPEN_REPARSE_POINT` and
//!   reject any reparse point (symlink, junction).
//! - [`is_reparse_point`]: `symlink_metadata`-based reparse check.
//! - [`current_user_sid_string`]: the caller's SID as `S-1-5-...` (cached).
//! - [`os_str_bytes`]: stable byte encoding of a path (UTF-16LE).

use std::collections::HashSet;
use std::ffi::{c_void, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
    HANDLE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL,
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    LockFileEx, MoveFileExW, ReplaceFileW, UnlockFileEx, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, READ_CONTROL,
    WRITE_DAC,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::IO::OVERLAPPED;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The user's home directory (`USERPROFILE`).
pub fn home_dir() -> Option<PathBuf> {
    env_path("USERPROFILE")
}

/// `%LOCALAPPDATA%`, falling back to `<home>\AppData\Local`.
pub fn local_app_data() -> Option<PathBuf> {
    env_path("LOCALAPPDATA").or_else(|| home_dir().map(|home| home.join("AppData").join("Local")))
}

/// `%APPDATA%`, falling back to `<home>\AppData\Roaming`.
pub fn app_data() -> Option<PathBuf> {
    env_path("APPDATA").or_else(|| home_dir().map(|home| home.join("AppData").join("Roaming")))
}

/// Stable byte encoding of an `OsStr` (UTF-16LE code units).
pub fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value
        .encode_wide()
        .flat_map(|unit| unit.to_le_bytes())
        .collect()
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ---------------------------------------------------------------- locking

/// Lock offset: far beyond any real content so the mandatory byte-range lock
/// never blocks reads or writes of the file itself.
const LOCK_OFFSET: u64 = 0x7FFF_FFFF_0000_0000;

fn lock_overlapped() -> OVERLAPPED {
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = LOCK_OFFSET as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (LOCK_OFFSET >> 32) as u32;
    overlapped
}

/// Take an exclusive lock on `file`. With `nonblocking`, contention returns
/// `ErrorKind::WouldBlock`; otherwise this blocks until the lock is free.
pub fn lock_exclusive(file: &File, nonblocking: bool) -> io::Result<()> {
    let mut flags = LOCKFILE_EXCLUSIVE_LOCK;
    if nonblocking {
        flags |= LOCKFILE_FAIL_IMMEDIATELY;
    }
    let mut overlapped = lock_overlapped();
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            flags,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, error));
    }
    Err(error)
}

/// Release the lock taken by [`lock_exclusive`]. Closing the handle (or dying)
/// releases it too, so errors here are advisory.
pub fn unlock(file: &File) {
    let mut overlapped = lock_overlapped();
    unsafe {
        UnlockFileEx(file.as_raw_handle() as HANDLE, 0, 1, 0, &mut overlapped);
    }
}

// ---------------------------------------------------------- atomic replace

fn transient(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
    )
}

fn retry(mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        match operation() {
            Err(error) if transient(&error) && attempt < 40 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

/// Rename `from` over `to` with POSIX semantics
/// (`FILE_RENAME_FLAG_POSIX_SEMANTICS | REPLACE_IF_EXISTS`): the new content
/// appears under the name in one namespace step, and the replaced file may
/// still be open (readers keep their handle). Unlike `MoveFileExW` /
/// `ReplaceFileW` a concurrent reader never sees a missing file or a sharing
/// violation. Needs Windows 10 1709+ on NTFS; `Err(Unsupported)` otherwise.
fn rename_posix(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FileRenameInfoEx, SetFileInformationByHandle, DELETE, FILE_RENAME_INFO,
    };
    const REPLACE_IF_EXISTS_AND_POSIX: u32 = 0x1 | 0x2;
    let source = OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(0x7)
        .open(from)?;
    let target = std::path::absolute(to)?;
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    let name_bytes = name.len() * 2;
    let total = std::mem::size_of::<FILE_RENAME_INFO>() + name_bytes;
    // 8-byte aligned backing store for the variable-length struct.
    let mut buffer = vec![0u64; total.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buffer` is zeroed, aligned, and at least `total` bytes, which
    // covers the fixed header plus the whole file name.
    unsafe {
        (*info).Anonymous.Flags = REPLACE_IF_EXISTS_AND_POSIX;
        (*info).RootDirectory = null_mut();
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        if SetFileInformationByHandle(
            source.as_raw_handle() as HANDLE,
            FileRenameInfoEx,
            info.cast::<c_void>(),
            total as u32,
        ) == 0
        {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error().map(|code| code as u32) {
                // Older Windows or a filesystem without POSIX rename.
                Some(1) | Some(50) | Some(87) => io::Error::new(io::ErrorKind::Unsupported, error),
                _ => error,
            });
        }
    }
    Ok(())
}

fn move_file_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide(from);
    let to = wide(to);
    retry(|| {
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

/// Atomically replace `to` with `from` (creating `to` if absent), writing
/// through to disk. No directory fsync exists or is needed on NTFS.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    match retry(|| rename_posix(from, to)) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => move_file_replace(from, to),
        other => other,
    }
}

/// Like [`replace_file`] but the target must exist at publication time:
/// a deleted target yields `ErrorKind::NotFound` and the staged file is left
/// for the caller's cleanup guard.
pub fn replace_existing_file(from: &Path, to: &Path) -> io::Result<()> {
    // Hold a handle on the target across the rename: a target deleted by
    // someone else is then delete-pending and refuses to be renamed over,
    // instead of being resurrected by the rename.
    let guard = open_existing_guard(to)?;
    let result = match retry(|| rename_posix(from, to)) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            drop(guard);
            return replace_existing_file_legacy(from, to);
        }
        Err(error) if transient(&error) => {
            // Delete-pending reads as access denied: report what it means.
            if !to.exists() {
                Err(io::Error::from(io::ErrorKind::NotFound))
            } else {
                Err(error)
            }
        }
        other => other,
    };
    drop(guard);
    result
}

fn open_existing_guard(to: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .access_mode(0x80) // FILE_READ_ATTRIBUTES
        .share_mode(0x7)
        .open(to)
}

fn replace_existing_file_legacy(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide(from);
    let to = wide(to);
    retry(|| {
        let ok = unsafe { ReplaceFileW(to.as_ptr(), from.as_ptr(), null(), 0, null(), null()) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

// -------------------------------------------------------- reparse points

fn attributes_are_reparse(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// True when `path` itself (not its target) is a reparse point.
pub fn is_reparse_point(path: &Path) -> io::Result<bool> {
    Ok(attributes_are_reparse(
        fs::symlink_metadata(path)?.file_attributes(),
    ))
}

/// Open `path` without following a final-component reparse point and reject
/// any reparse point outright. The Windows analogue of `O_NOFOLLOW`.
pub fn open_no_follow(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    let file = options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if attributes_are_reparse(file.metadata()?.file_attributes()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a reparse point", path.display()),
        ));
    }
    Ok(file)
}

// -------------------------------------------------------------- identity

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The current user's SID as a string, computed once.
pub fn current_user_sid_string() -> io::Result<String> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(sid) = SID.get() {
        return Ok(sid.clone());
    }
    let sid = query_user_sid_string()?;
    Ok(SID.get_or_init(|| sid).clone())
}

fn query_user_sid_string() -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let mut needed = 0u32;
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 backing keeps the TOKEN_USER header aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let mut text: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    let value = OsString::from_wide(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe { LocalFree(text.cast()) };
    value
        .into_string()
        .map_err(|_| io::Error::other("SID string is not valid Unicode"))
}

// ------------------------------------------------------------ private dirs

/// Give the directory open as `handle` (needs `WRITE_DAC`) a protected DACL
/// granting full control, inherited by children, to the current user only.
pub fn harden_dir_handle(handle: RawHandle) -> io::Result<()> {
    let sid = current_user_sid_string()?;
    let sddl: Vec<u16> = format!("D:PAI(A;OICI;FA;;;{sid})")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut present = 0i32;
        let mut defaulted = 0i32;
        let mut dacl: *mut ACL = null_mut();
        if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
            == 0
            || present == 0
        {
            return Err(io::Error::last_os_error());
        }
        let status = unsafe {
            SetSecurityInfo(
                handle as HANDLE,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    })();
    unsafe { LocalFree(descriptor) };
    result
}

/// True when the object open as `handle` is owned by the current user or by
/// BUILTIN\Administrators (what an elevated process creates).
fn handle_owned_by_user(handle: RawHandle) -> io::Result<bool> {
    let mut owner: *mut c_void = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let matches_sid = |text: &str| -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sid: *mut c_void = null_mut();
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
            return false;
        }
        let equal = unsafe { EqualSid(owner, sid) } != 0;
        unsafe { LocalFree(sid) };
        equal
    };
    let user = current_user_sid_string()?;
    let trusted = matches_sid(&user) || matches_sid("S-1-5-32-544");
    unsafe { LocalFree(descriptor) };
    Ok(trusted)
}

fn trusted_roots() -> &'static Vec<PathBuf> {
    static ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut roots = vec![std::env::temp_dir()];
        roots.extend(home_dir());
        roots.extend(local_app_data());
        roots.extend(app_data());
        roots
    })
}

/// Ancestors of the user's profile/temp roots may legitimately be junctions
/// (redirected profiles, `subst`), so only components strictly below them are
/// checked for reparse points.
fn is_trusted_ancestor(path: &Path) -> bool {
    trusted_roots().iter().any(|root| root.starts_with(path))
}

fn hardened() -> &'static Mutex<HashSet<PathBuf>> {
    static SET: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Create `path` (and parents) as a private directory: no reparse points in
/// any component below the profile/temp roots or in the leaf, and the leaf
/// carries an owner-only protected DACL (applied when we create it or have
/// not hardened it in this process yet).
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory path is empty",
        ));
    }
    let mut current = PathBuf::new();
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let last = components.peek().is_none();
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component.as_os_str());
                continue;
            }
            Component::CurDir => continue,
            other => current.push(other.as_os_str()),
        }
        if matches!(component, Component::ParentDir) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() && !attributes_are_reparse(metadata.file_attributes()) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} is not a directory", current.display()),
                    ));
                }
                if attributes_are_reparse(metadata.file_attributes())
                    && (last || !is_trusted_ancestor(&current))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is a reparse point", current.display()),
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                if is_reparse_point(&current)? {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is a reparse point", current.display()),
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    harden_leaf(&current)
}

fn harden_leaf(path: &Path) -> io::Result<()> {
    if hardened()
        .lock()
        .map(|set| set.contains(path))
        .unwrap_or(false)
        && path.is_dir()
    {
        return Ok(());
    }
    let directory = OpenOptions::new()
        .read(true)
        .access_mode(
            READ_CONTROL | WRITE_DAC | 0x0001, /* FILE_LIST_DIRECTORY */
        )
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if attributes_are_reparse(directory.metadata()?.file_attributes()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a reparse point", path.display()),
        ));
    }
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a real directory", path.display()),
        ));
    }
    if !handle_owned_by_user(directory.as_raw_handle())? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by another user", path.display()),
        ));
    }
    harden_dir_handle(directory.as_raw_handle())?;
    if let Ok(mut set) = hardened().lock() {
        set.insert(path.to_path_buf());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn lock_is_exclusive_and_reports_would_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let a = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let b = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        lock_exclusive(&a, true).unwrap();
        let error = lock_exclusive(&b, true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        // Content stays readable/writable while locked.
        let mut c = OpenOptions::new().write(true).open(&path).unwrap();
        c.write_all(b"hi").unwrap();
        unlock(&a);
        lock_exclusive(&b, true).unwrap();
    }

    #[test]
    fn lock_released_when_handle_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("y.lock");
        let a = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        lock_exclusive(&a, true).unwrap();
        drop(a);
        let b = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        lock_exclusive(&b, true).unwrap();
    }

    #[test]
    fn replace_file_overwrites_and_creates() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.tmp");
        let to = dir.path().join("a");
        fs::write(&from, b"new").unwrap();
        replace_file(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"new");
        fs::write(&from, b"newer").unwrap();
        replace_file(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"newer");
    }

    #[test]
    fn replace_existing_requires_target() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.tmp");
        let to = dir.path().join("a");
        fs::write(&from, b"new").unwrap();
        let error = replace_existing_file(&from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        fs::write(&to, b"old").unwrap();
        replace_existing_file(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
    }

    #[test]
    fn private_dir_is_created_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b");
        ensure_private_dir(&target).unwrap();
        ensure_private_dir(&target).unwrap();
        assert!(target.is_dir());
        // Children inherit the owner-only DACL and stay usable by us.
        fs::write(target.join("f"), b"x").unwrap();
        assert_eq!(fs::read(target.join("f")).unwrap(), b"x");
        // Protected, owner-only: exactly one ACE, inherited by children, no
        // Users/Everyone/Administrators grants.
        let out = std::process::Command::new("icacls")
            .arg(&target)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let aces = text.matches("(OI)(CI)(F)").count();
        assert_eq!(aces, 1, "{text}");
        assert!(
            !text.contains("Everyone") && !text.contains("BUILTIN\\Users"),
            "{text}"
        );
    }

    #[test]
    fn private_dir_rejects_junction_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(ensure_private_dir(&link).is_err());
        assert!(is_reparse_point(&link).unwrap());
        assert!(!is_reparse_point(&real).unwrap());
    }

    #[test]
    fn private_dir_rejects_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"x").unwrap();
        assert!(ensure_private_dir(&file).is_err());
    }

    #[test]
    fn sid_string_looks_like_a_sid() {
        assert!(current_user_sid_string().unwrap().starts_with("S-1-5-"));
    }
}
