//! Session directory layout: the `Paths` triple, hardened creation of
//! private runtime/state directories (symlink and permission checks), and
//! workspace canonicalization.

use anyhow::{anyhow, Context, Result};
#[cfg(unix)]
use anyhow::bail;
use std::env;
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Paths {
    pub runtime_root: PathBuf,
    pub state_root: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        Self::discover_with(None, None, None)
    }

    /// [`Paths::discover`] with explicit overrides that take precedence
    /// over the `APLEXER_*` and XDG environment (the Python bindings'
    /// keyword arguments). A relative override resolves against the
    /// current directory, like its `APLEXER_*` counterpart.
    pub fn discover_with(
        runtime: Option<&Path>,
        state: Option<&Path>,
        config: Option<&Path>,
    ) -> Result<Self> {
        let runtime_root = if let Some(value) = runtime {
            absolute_override_path(value.to_path_buf(), "runtime_dir")?
        } else if let Some(value) = env::var_os("APLEXER_RUNTIME_DIR") {
            absolute_override_path(PathBuf::from(value), "APLEXER_RUNTIME_DIR")?
        } else {
            default_runtime_root()?
        };
        let state_root = if let Some(value) = state {
            absolute_override_path(value.to_path_buf(), "state_dir")?
        } else if let Some(value) = env::var_os("APLEXER_STATE_DIR") {
            absolute_override_path(PathBuf::from(value), "APLEXER_STATE_DIR")?
        } else {
            default_state_root()?
        };
        let config_file = if let Some(value) = config {
            absolute_override_path(value.to_path_buf(), "config")?
        } else if let Some(value) = env::var_os("APLEXER_CONFIG") {
            absolute_override_path(PathBuf::from(value), "APLEXER_CONFIG")?
        } else {
            default_config_file()?
        };
        let paths = Self {
            runtime_root,
            state_root,
            config_file,
        };
        paths.ensure()?;
        Ok(paths)
    }

    pub fn ensure(&self) -> Result<()> {
        ensure_private_dir(&self.runtime_root)?;
        ensure_private_dir(&self.runtime_root.join("sessions"))?;
        ensure_private_dir(&self.state_root)?;
        ensure_private_dir(&self.state_root.join("sessions"))?;
        Ok(())
    }

    pub fn runtime_session(&self, id: Uuid) -> PathBuf {
        self.runtime_root.join("sessions").join(id.to_string())
    }
    pub fn state_session(&self, id: Uuid) -> PathBuf {
        self.state_root.join("sessions").join(id.to_string())
    }
    /// Unix: the control socket node. Windows: the named-pipe name (the pipe
    /// namespace has no filesystem path; see sys::windows::ipc::pipe_name).
    pub fn socket(&self, id: Uuid) -> PathBuf {
        #[cfg(windows)]
        {
            crate::sys::windows::ipc::pipe_name(id)
                .unwrap_or_else(|_| PathBuf::from(format!(r"\\.\pipe\aplexer-unknown-{id}")))
        }
        #[cfg(not(windows))]
        {
            self.runtime_session(id).join("control.sock")
        }
    }
    pub fn record(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("session.json")
    }
    pub fn history(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("history.bin")
    }
    /// The dead-session fallback for `a capture --screen` (design doc
    /// section 5.5) -- the plain-text screen as it looked the moment the
    /// worker exited, written once by `OutputHub::finish`.
    pub fn screen_txt(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("screen.txt")
    }
    pub fn worker_lock(&self, id: Uuid) -> PathBuf {
        self.runtime_session(id).join("worker.lock")
    }
    pub fn registry_lock(&self) -> PathBuf {
        self.state_root.join("registry.lock")
    }
    /// Last per-session process sample (`proc_usage`). Runtime, not state:
    /// it is a rate baseline, and losing it only hides CPU until the next
    /// pair of samples.
    pub fn proc_usage_cache(&self) -> PathBuf {
        self.runtime_root.join("proc-usage.json")
    }
    /// Finished-session tombstones and superseded-session archives live
    /// here; both answer "this id is deliberately gone".
    pub fn retired_sessions_dir(&self) -> PathBuf {
        self.state_root.join("retired-sessions")
    }
    pub fn retired_session(&self, id: Uuid) -> PathBuf {
        self.retired_sessions_dir().join(id.to_string())
    }
    /// Ack-gated crash/OOM warnings (`crate::warnings`), one JSON sidecar
    /// per session id. Deliberately outside each session's state dir: the
    /// session's own removal (`a prune`, the list sweep, worker teardown)
    /// is exactly the event the warning must survive.
    pub fn warnings_dir(&self) -> PathBuf {
        self.state_root.join("warnings")
    }
}

pub(crate) fn absolute_override_path(path: PathBuf, variable: &str) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(env::current_dir()
        .with_context(|| format!("resolve relative {variable}"))?
        .join(path))
}

#[cfg(unix)]
pub(crate) fn absolute_xdg_path(path: PathBuf, variable: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!(
            "{variable} must be an absolute path, got {}",
            path.display()
        );
    }
    Ok(path)
}

/// The user's home directory: `HOME` on Unix, `USERPROFILE` on Windows.
#[cfg(unix)]
pub(crate) fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

#[cfg(windows)]
pub(crate) fn home_dir() -> Result<PathBuf> {
    crate::sys::windows::fs::home_dir().ok_or_else(|| anyhow!("USERPROFILE is not set"))
}

#[cfg(unix)]
fn default_runtime_root() -> Result<PathBuf> {
    if let Some(value) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(absolute_xdg_path(PathBuf::from(value), "XDG_RUNTIME_DIR")?.join("aplexer"));
    }
    let uid = unsafe { libc::geteuid() };
    Ok(PathBuf::from(format!("/tmp/aplexer-{uid}")))
}

#[cfg(unix)]
fn default_state_root() -> Result<PathBuf> {
    if let Some(value) = env::var_os("XDG_STATE_HOME") {
        return Ok(absolute_xdg_path(PathBuf::from(value), "XDG_STATE_HOME")?.join("aplexer"));
    }
    Ok(home_dir()?.join(".local/state/aplexer"))
}

#[cfg(unix)]
fn default_config_file() -> Result<PathBuf> {
    if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(
            absolute_xdg_path(PathBuf::from(value), "XDG_CONFIG_HOME")?.join("aplexer/config.toml")
        );
    }
    Ok(home_dir()?.join(".config/aplexer/config.toml"))
}

/// `%LOCALAPPDATA%\aplexer\run`.
#[cfg(windows)]
fn default_runtime_root() -> Result<PathBuf> {
    Ok(windows_local_root()?.join("run"))
}

/// `%LOCALAPPDATA%\aplexer\state`.
#[cfg(windows)]
fn default_state_root() -> Result<PathBuf> {
    Ok(windows_local_root()?.join("state"))
}

/// `%APPDATA%\aplexer\config.toml`.
#[cfg(windows)]
fn default_config_file() -> Result<PathBuf> {
    let base = crate::sys::windows::fs::app_data().ok_or_else(|| anyhow!("APPDATA is not set"))?;
    Ok(base.join("aplexer").join("config.toml"))
}

#[cfg(windows)]
fn windows_local_root() -> Result<PathBuf> {
    let base = crate::sys::windows::fs::local_app_data()
        .ok_or_else(|| anyhow!("LOCALAPPDATA is not set"))?;
    Ok(base.join("aplexer"))
}

/// Windows: owner-only protected DACL, reparse points rejected.
#[cfg(windows)]
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    crate::sys::windows::fs::ensure_private_dir(path)
        .with_context(|| format!("create private directory {}", path.display()))
}

#[cfg(unix)]
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("directory path is empty");
    }

    // Walk the path a component at a time. `create_dir_all` followed by a
    // path-based chmod leaves a check/use gap in which the final component
    // can be replaced with a symlink, causing us to chmod its target. Keeping
    // every component pinned by a directory fd, and refusing symlinks at each
    // `openat`, makes both creation and the eventual chmod refer to the inode
    // we actually inspected.
    let base = if path.is_absolute() { "/" } else { "." };
    let mut directory = open_directory_at(libc::AT_FDCWD, OsStr::new(base))
        .with_context(|| format!("open directory base for {}", path.display()))?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => OsStr::new(".."),
            Component::Normal(name) => name,
            Component::Prefix(_) => bail!("unexpected prefix in Unix path {}", path.display()),
        };
        let name_c = CString::new(name.as_bytes()).context("directory component contains NUL")?;
        let next = match open_directory_at(directory.as_raw_fd(), name) {
            Ok(next) => next,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                if unsafe { libc::mkdirat(directory.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
                    let mkdir_error = io::Error::last_os_error();
                    if mkdir_error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(mkdir_error)
                            .with_context(|| format!("create {}", path.display()));
                    }
                }
                open_directory_at(directory.as_raw_fd(), name)
                    .with_context(|| format!("open newly-created {}", path.display()))?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("open {} without following symbolic links", path.display())
                });
            }
        };
        directory = next;
    }

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(directory.as_raw_fd(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("inspect {}", path.display()));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        bail!("{} is not a real directory", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    if stat.st_uid != uid {
        bail!(
            "{} is owned by uid {}, expected {}",
            path.display(),
            stat.st_uid,
            uid
        );
    }
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("chmod 0700 {}", path.display()));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn open_directory_at(parent_fd: RawFd, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    fs::canonicalize(&absolute).or_else(|_| {
        let parent = absolute
            .parent()
            .ok_or_else(|| anyhow!("invalid workspace"))?;
        let leaf = absolute
            .file_name()
            .ok_or_else(|| anyhow!("invalid workspace"))?;
        Ok::<PathBuf, anyhow::Error>(fs::canonicalize(parent)?.join(leaf))
    })
}
