//! Platform seams for the native Windows and macOS ports. See
//! docs/windows-port.md and docs/macos-port.md.
//!
//! Unix code stays where it is, behind `#[cfg(unix)]`. Windows implementations
//! live in `sys::windows::*` and are called from `#[cfg(windows)]` branches;
//! macOS libproc/sysctl helpers live in `sys::macos::*` and are called from
//! `#[cfg(target_os = "macos")]` branches.

#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "macos")]
pub mod macos;

pub mod ipc;
