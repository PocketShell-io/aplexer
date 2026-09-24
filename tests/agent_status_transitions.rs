//! Agent transitions → status-bar immediacy, end to end: a client is
//! attached to a plain shell session, and an agent is started (and later
//! killed) *inside* that session through the attached keyboard -- the exact
//! shape of "I ran claude in my shell, exited, started codex". Detection is
//! query-time, so nothing can push this fact; the client's status thread
//! re-runs the `/proc` walk every tick and must repaint the agent segment
//! on the transition itself, not coast until a TTL refresh happens to
//! coincide with a draw.
//!
//! Harness mirrors tests/rename_status_propagation.rs.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    config_file: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = TempDir::new().expect("runtime tempdir");
        let state_dir = TempDir::new().expect("state tempdir");
        let config_file = runtime_dir.path().join("config.toml");
        Self {
            runtime_dir,
            state_dir,
            config_file,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        cmd.env("APLEXER_RUNTIME_DIR", self.runtime_dir.path());
        cmd.env("APLEXER_STATE_DIR", self.state_dir.path());
        cmd.env("APLEXER_CONFIG", &self.config_file);
        cmd
    }

    fn run_ok(&self, args: &[&str], timeout: Duration) -> String {
        let mut cmd = self.command();
        cmd.args(args).stdin(Stdio::null());
        let output = run_with_timeout(cmd, timeout);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().expect("failed to spawn command");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("failed to wait for command: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("command (pid {pid}) did not finish within {timeout:?}");
        }
    }
}

struct PtyClient {
    child: std::process::Child,
    master: std::fs::File,
    captured: Arc<Mutex<Vec<u8>>>,
}

impl PtyClient {
    fn spawn(harness: &Harness, id: &str, rows: u16, cols: u16) -> Self {
        let (master, slave) = aplexer::open_pty(rows, cols).expect("open pty");
        let mut cmd = harness.command();
        cmd.args(["attach", id]);
        cmd.stdin(Stdio::from(slave.try_clone().expect("dup slave for stdin")));
        cmd.stdout(Stdio::from(
            slave.try_clone().expect("dup slave for stdout"),
        ));
        cmd.stderr(Stdio::from(
            slave.try_clone().expect("dup slave for stderr"),
        ));
        let child = cmd.spawn().expect("spawn `a attach` on a pty");
        drop(slave);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let mut reader = master.try_clone().expect("dup pty master for reading");
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Err(_) => break,
                    Ok(n) => sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            child,
            master,
            captured,
        }
    }

    fn output(&self) -> Vec<u8> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Type a command line into the attached session (type-through).
    fn type_line(&mut self, line: &str) {
        self.master
            .write_all(format!("{line}\r").as_bytes())
            .expect("write to pty master");
    }

    /// The host terminal's reserved status row, as the bytes the client
    /// actually wrote render at the physical size -- the same differential
    /// surface tests/rename_status_propagation.rs asserts on.
    fn status_row(&self, rows: u16, cols: u16) -> String {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(&self.output());
        let screen = parser.screen();
        screen
            .contents_between(rows - 1, 0, rows - 1, cols)
            .trim_end()
            .to_string()
    }

    fn wait_for_status_row(&self, needle: &str, what: &str) {
        self.wait_for_status_row_within(needle, what, Duration::from_secs(10));
    }

    /// The immediacy half of the contract: a transition must reach the bar
    /// within the bound, not eventually. The detection tick is 150 ms and a
    /// redraw lands on the same tick, so 3 s is generous headroom for CI --
    /// and still shorter than the old worst case (an idle session coasting
    /// on its 1 s TTL + 3 s overdue draw).
    fn wait_for_status_row_within(&self, needle: &str, what: &str, within: Duration) {
        let deadline = Instant::now() + within;
        let mut last = self.status_row(ROWS, COLS);
        while !last.contains(needle) {
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; last status row: {last:?}");
            }
            thread::sleep(Duration::from_millis(50));
            last = self.status_row(ROWS, COLS);
        }
    }

    fn detach(mut self) {
        self.master
            .write_all(&[0x02, b'd'])
            .expect("write detach chord");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The worker's model is the physical terminal minus the reserved status row.
const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Starting an agent inside an attached shell session names the bar after
/// it within a tick or two; killing it drops back to `shell` just as fast.
#[test]
fn agent_start_and_exit_update_the_attached_status_bar_immediately() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");

    let stdout = harness.run_ok(
        &[
            "--json",
            "start",
            "--workspace",
            workspace.path().to_str().expect("utf8 workspace"),
            "--tag",
            "sh",
            "--",
            "/bin/bash",
            "--norc",
        ],
        Duration::from_secs(15),
    );
    let record: serde_json::Value = serde_json::from_str(&stdout).expect("start JSON");
    let id = record["id"].as_str().expect("session id").to_string();

    let mut client = PtyClient::spawn(&harness, &id, ROWS, COLS);
    client.wait_for_status_row("shell", "the bare-shell bar");

    // Start an agent inside the session, the way a user does: type it into
    // the attached keyboard. `sleep` under the name `codex` classifies as
    // codex by its comm, the same evidence the real binary presents.
    client.type_line("cp /bin/sleep codex && ./codex 300 &");

    client.wait_for_status_row_within(
        "codex",
        "the started agent on the bar",
        Duration::from_secs(3),
    );
    let row = client.status_row(ROWS, COLS);
    assert!(
        !row.contains("shell"),
        "the bare-shell label must be gone while the agent runs: {row:?}"
    );

    // And the exit: the agent leaves the tree, the bar returns to shell.
    client.type_line("kill %1");

    client.wait_for_status_row_within(
        "shell",
        "the shell label after the agent exits",
        Duration::from_secs(3),
    );
    let row = client.status_row(ROWS, COLS);
    assert!(
        !row.contains("codex"),
        "the dead agent must be gone from the bar: {row:?}"
    );

    client.detach();
    harness.run_ok(
        &["kill", &id, "--signal", "KILL", "--grace-ms", "0"],
        Duration::from_secs(5),
    );
}
