//! Windows implementations of the platform seam. One file per owner; see
//! docs/windows-port.md for who owns what.

pub mod console;
pub mod fs;
pub mod ipc;
pub mod job;
pub mod launch;
pub mod procinfo;
pub mod pty;
pub mod registry;
pub mod signal;
