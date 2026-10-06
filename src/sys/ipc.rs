//! Cross-platform names for the control/attach transport. Unix: Unix domain
//! sockets. Windows: named pipes (`sys::windows::ipc`). The rest of the crate
//! says `Stream` / `Listener` and never names the platform type.

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(windows)]
pub use super::windows::ipc::{Listener, Stream};
