//! Platform seam for the native Windows port. See docs/windows-port.md.
//!
//! Unix code stays where it is, behind `#[cfg(unix)]`. Windows implementations
//! live in `sys::windows::*` and are called from `#[cfg(windows)]` branches.

#[cfg(windows)]
pub mod windows;

pub mod ipc;
