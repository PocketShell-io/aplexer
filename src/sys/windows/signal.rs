//! Signal abstraction: map wire i32 signals to Interrupt/Terminate/Kill and
//! reject the rest. Owner: agent "job" (shared with process/worker call sites).
//!
//! The wire protocol keeps Linux signal numbers (`i32`). On Windows:
//!
//! * `0` probes liveness (no-op, success);
//! * `INT` (2) and `TERM` (15) are graceful: the caller-supplied callback
//!   writes `0x03` (Ctrl-C) to the PTY input so the workload's console sees
//!   an interrupt;
//! * `KILL` (9) terminates the whole session job;
//! * everything else (HUP, QUIT, USR1, USR2, STOP, CONT, ...) is rejected
//!   with [`UnsupportedSignal`], a clear "unsupported on Windows" error.

use super::job::{Job, KILLED_EXIT_CODE};
use std::fmt;
use std::io;

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGKILL: i32 = 9;
pub const SIGUSR1: i32 = 10;
pub const SIGUSR2: i32 = 12;
pub const SIGTERM: i32 = 15;

/// The byte a PTY input writer sends for a graceful signal.
pub const CTRL_C: u8 = 0x03;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Signal 0: existence probe.
    Probe,
    /// INT: write Ctrl-C.
    Interrupt,
    /// TERM: write Ctrl-C (Windows has no other graceful request for a
    /// console workload; escalation to Kill is the caller's grace timer).
    Terminate,
    /// KILL: `TerminateJobObject`.
    Kill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedSignal(pub i32);

impl fmt::Display for UnsupportedSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "signal {} ({}) is not supported on Windows; use TERM, INT or KILL",
            self.0,
            signal_name(self.0)
        )
    }
}

impl std::error::Error for UnsupportedSignal {}

pub fn signal_name(signal: i32) -> &'static str {
    match signal {
        0 => "0",
        SIGHUP => "HUP",
        SIGINT => "INT",
        SIGQUIT => "QUIT",
        SIGKILL => "KILL",
        SIGUSR1 => "USR1",
        SIGUSR2 => "USR2",
        SIGTERM => "TERM",
        _ => "unknown",
    }
}

impl Signal {
    pub fn from_wire(signal: i32) -> Result<Self, UnsupportedSignal> {
        match signal {
            0 => Ok(Self::Probe),
            SIGINT => Ok(Self::Interrupt),
            SIGTERM => Ok(Self::Terminate),
            SIGKILL => Ok(Self::Kill),
            other => Err(UnsupportedSignal(other)),
        }
    }

    pub fn wire(self) -> i32 {
        match self {
            Self::Probe => 0,
            Self::Interrupt => SIGINT,
            Self::Terminate => SIGTERM,
            Self::Kill => SIGKILL,
        }
    }

    pub fn is_graceful(self) -> bool {
        matches!(self, Self::Interrupt | Self::Terminate)
    }
}

/// Deliver a wire signal to a session. `write_input` writes bytes to the
/// session's PTY input (only called for graceful signals); `job` is the
/// session's containment job (only used for KILL).
pub fn deliver(
    signal: i32,
    job: &Job,
    write_input: impl FnOnce(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    match Signal::from_wire(signal).map_err(io::Error::other)? {
        Signal::Probe => Ok(()),
        Signal::Interrupt | Signal::Terminate => write_input(&[CTRL_C]),
        Signal::Kill => job.terminate(KILLED_EXIT_CODE),
    }
}

type InputWriter = Box<dyn Fn(&[u8]) -> io::Result<()> + Send + Sync>;

static INPUT_WRITER: std::sync::OnceLock<InputWriter> = std::sync::OnceLock::new();

/// Register, once, the function that writes bytes to the workload's PTY
/// input. The worker installs it as soon as the PTY exists; every graceful
/// signal then turns into a Ctrl-C byte through it. Returns false if a writer
/// was already installed.
pub fn install_input_writer(
    writer: impl Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
) -> bool {
    INPUT_WRITER.set(Box::new(writer)).is_ok()
}

/// Write Ctrl-C through the installed PTY input writer.
pub fn write_graceful_input() -> io::Result<()> {
    match INPUT_WRITER.get() {
        Some(writer) => writer(&[CTRL_C]),
        None => Err(io::Error::other(
            "no PTY input writer is installed for graceful signals",
        )),
    }
}

/// [`deliver`] using the installed PTY input writer.
pub fn deliver_installed(signal: i32, job: &Job) -> io::Result<()> {
    deliver(signal, job, |_| write_graceful_input())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_wire_signals() {
        assert_eq!(Signal::from_wire(0), Ok(Signal::Probe));
        assert_eq!(Signal::from_wire(2), Ok(Signal::Interrupt));
        assert_eq!(Signal::from_wire(15), Ok(Signal::Terminate));
        assert_eq!(Signal::from_wire(9), Ok(Signal::Kill));
        for unsupported in [SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2, 19, 18] {
            let error = Signal::from_wire(unsupported).unwrap_err();
            assert!(error.to_string().contains("not supported on Windows"));
        }
        assert!(Signal::Terminate.is_graceful() && !Signal::Kill.is_graceful());
        assert_eq!(Signal::Kill.wire(), 9);
    }

    #[test]
    fn graceful_writes_ctrl_c_and_kill_terminates_job() {
        let job = Job::create(
            format!("sigtest-{}", std::process::id()),
            &Default::default(),
        )
        .unwrap();
        let mut seen = Vec::new();
        deliver(SIGTERM, &job, |bytes| {
            seen.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [CTRL_C]);
        assert!(deliver(SIGHUP, &job, |_| panic!("no write")).is_err());
        deliver(SIGKILL, &job, |_| panic!("no write")).unwrap();
    }
}
