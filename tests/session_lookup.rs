//! Bare-tag session lookup: every subcommand whose help promises "tag in
//! the current workspace" must keep that promise -- `a agent` and
//! `a rename` used to call the machine-facing `resolve_record` directly
//! and answer "no matching session" for exactly that form, while
//! `a status` / `a capture` / `a send` resolved it fine through `resolve`.
//!
//! The "current workspace" here is the caller's session binding
//! (`APLEXER_SESSION_ID` -> the session's own workspace), the first leg of
//! `resolve_message_workspace` and the one every in-session agent relies
//! on; the cwd/`APLEXER_WORKSPACE` legs share the same tag-matching body
//! (`resolve_tag_in_workspace`), which the unit tests in
//! `src/bin/aplexer_tests/selector.rs` cover with an explicit workspace.
//!
//! Registry shape rules pinned here: a bare tag resolves to the single
//! live holder even when a corpse still holds the same pair (`a rename`'s
//! issue-#13 window), an ambiguity error names every candidate by short
//! UUID prefix instead of a bare "no matching session", and the same
//! candidate listing covers `workspace:tag` selectors in `resolve_record`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    workspace: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let config = state.path().join("config.toml");
        let workspace = state.path().join("ws");
        fs::create_dir_all(&workspace).expect("workspace dir");
        Self {
            runtime,
            state,
            config,
            workspace,
        }
    }

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        }
    }

    /// One record in the harness registry, with its durable state laid out
    /// so `read_session_record`'s identity checks accept it.
    fn seed(&self, tag: &str, phase: Phase) -> SessionRecord {
        let id = Uuid::new_v4();
        let paths = self.paths();
        let record = SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: self.workspace.clone(),
            tag: tag.to_owned(),
            engine: "codex".into(),
            profile: None,
            command: vec!["/bin/sh".into()],
            cwd: self.workspace.clone(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: 4096,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase,
            worker_pid: None,
            agent_override: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        };
        self.write(record)
    }

    /// A live variant of a seeded record: a live worker pid (this test
    /// process -- never reaped mid-run) and a containment that keeps the
    /// record on every surface.
    fn make_live(&self, mut record: SessionRecord) -> SessionRecord {
        record.worker_pid = Some(std::process::id());
        record.workload_pid = None;
        self.write(record)
    }

    /// The corpse shape `reap_verdict` accepts: no worker, no leader,
    /// containment proven empty, terminal phase.
    fn make_corpse(&self, mut record: SessionRecord) -> SessionRecord {
        record.phase = Phase::Exited;
        record.worker_pid = None;
        record.workload_pid = None;
        record.containment_empty = Some(true);
        record.exit = Some(aplexer::ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 1,
        });
        self.write(record)
    }

    fn write(&self, record: SessionRecord) -> SessionRecord {
        let paths = self.paths();
        paths.ensure().expect("ensure paths");
        fs::create_dir_all(paths.state_session(record.id)).expect("session dir");
        fs::write(&record.history_path, b"").expect("history file");
        atomic_write_json(&paths.record(record.id), &record).expect("write record");
        record
    }

    /// Run `a <args>` bound to `caller`'s session -- the in-session shape
    /// every aplexer-hosted agent runs under. The explicit binding is the
    /// point: it overrides any inherited environment deterministically.
    fn run_bound(&self, caller: &SessionRecord, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .env("APLEXER_SESSION_ID", caller.id.to_string())
            .args(args);
        run_with_timeout(command, Duration::from_secs(30))
    }

    fn run_ok(&self, caller: &SessionRecord, args: &[&str]) -> String {
        let output = self.run_bound(caller, args);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> std::process::Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn command");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("wait for command: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("command pid {pid} exceeded timeout {timeout:?}");
        }
    }
}

/// The reported failure, verbatim: from inside a session whose workspace
/// holds the tag, `a agent codex-principal` must answer like
/// `a agent <workspace>:codex-principal` always has -- not "no matching
/// session".
#[test]
fn a_bare_tag_resolves_for_agent_in_the_bound_workspace() {
    let harness = Harness::new();
    let caller = harness.seed("caller", Phase::Running);
    harness.seed("codex-principal", Phase::Running);

    let stdout = harness.run_ok(&caller, &["agent", "codex-principal"]);
    assert!(
        stdout.contains("codex-principal"),
        "the bare tag must resolve to its session: {stdout}"
    );

    // The JSON show form carries the same answer, and the machine-facing
    // workspace:tag form agrees with it.
    let shown: Value =
        serde_json::from_str(&harness.run_ok(&caller, &["agent", "codex-principal", "--json"]))
            .expect("agent show JSON");
    let by_pair: Value = serde_json::from_str(&harness.run_ok(
        &caller,
        &[
            "agent",
            &format!("{}:codex-principal", harness.workspace.display()),
            "--json",
        ],
    ))
    .expect("agent ws:tag JSON");
    assert_eq!(
        shown["id"], by_pair["id"],
        "both forms name the same session"
    );
}

/// The tag still means the live session when a corpse holds the same pair:
/// the `a rename` window (issue #13) must not turn `a agent <tag>` into an
/// ambiguity error, and must not answer with the corpse either.
#[test]
fn a_bare_tag_prefers_the_live_holder_over_a_corpse() {
    let harness = Harness::new();
    let caller = harness.seed("caller", Phase::Running);
    let corpse = harness.make_corpse(harness.seed("codex-principal", Phase::Running));
    let live = harness.make_live(harness.seed("codex-principal", Phase::Running));

    let shown: Value =
        serde_json::from_str(&harness.run_ok(&caller, &["agent", "codex-principal", "--json"]))
            .expect("agent show JSON");
    assert_eq!(
        shown["id"],
        Value::String(live.id.to_string()),
        "the bare tag answered with the corpse {corpse:?}"
    );
}

/// No live holder to disambiguate: the error must name every candidate by
/// its short UUID prefix -- the shortest selector that still resolves --
/// rather than a bare "no matching session".
#[test]
fn an_ambiguous_bare_tag_names_its_candidates() {
    let harness = Harness::new();
    let caller = harness.seed("caller", Phase::Running);
    let first = harness.make_live(harness.seed("codex-principal", Phase::Running));
    let second = harness.make_live(harness.seed("codex-principal", Phase::Running));

    let output = harness.run_bound(&caller, &["agent", "codex-principal"]);
    assert!(
        !output.status.success(),
        "a two-live-holder tag must fail loudly: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for candidate in [
        &first.id.simple().to_string()[..8],
        &second.id.simple().to_string()[..8],
    ] {
        assert!(
            stderr.contains(candidate),
            "the ambiguity error must list candidate {candidate}: {stderr}"
        );
    }
}

/// The same candidate listing for the machine-facing `workspace:tag`
/// selector form inside `resolve_record` -- no live holder, no answer.
#[test]
fn an_ambiguous_workspace_tag_selector_names_its_candidates() {
    let harness = Harness::new();
    let caller = harness.seed("caller", Phase::Running);
    let first = harness.make_live(harness.seed("codex-principal", Phase::Running));
    let second = harness.make_live(harness.seed("codex-principal", Phase::Running));

    let output = harness.run_bound(
        &caller,
        &[
            "agent",
            &format!("{}:codex-principal", harness.workspace.display()),
        ],
    );
    assert!(
        !output.status.success(),
        "a two-live-holder pair must fail loudly: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ambiguous"),
        "the error says what is wrong: {stderr}"
    );
    for candidate in [
        &first.id.simple().to_string()[..8],
        &second.id.simple().to_string()[..8],
    ] {
        assert!(
            stderr.contains(candidate),
            "the ambiguity error must list candidate {candidate}: {stderr}"
        );
    }
}

/// The subcommands the brief asked to check share the resolution: `a
/// status <tag>` (and through the same `resolve`, `a capture`/`a send`)
/// keep resolving the bare tag as before.
#[test]
fn a_bare_tag_still_resolves_for_status() {
    let harness = Harness::new();
    let caller = harness.seed("caller", Phase::Running);
    harness.seed("codex-principal", Phase::Running);

    let stdout = harness.run_ok(&caller, &["status", "codex-principal", "--json"]);
    let status: Value = serde_json::from_str(&stdout).expect("status JSON");
    assert_eq!(
        status["tag"],
        Value::String("codex-principal".into()),
        "status resolved the wrong session: {stdout}"
    );
}

/// A tag that exists only on corpses resolves too -- it addresses a real
/// (terminal) record -- while a tag that never existed keeps the tag-shaped
/// error, not a bare "no matching session".
#[test]
fn a_corpse_only_tag_resolves_and_an_unknown_tag_names_the_workspace() {
    let harness = Harness::new();
    let caller = harness.seed("caller", Phase::Running);
    harness.make_corpse(harness.seed("gone", Phase::Running));

    let shown: Value = serde_json::from_str(&harness.run_ok(&caller, &["agent", "gone", "--json"]))
        .expect("agent show JSON");
    assert_eq!(shown["agent"], Value::Null, "a corpse detects no agent");

    let output = harness.run_bound(&caller, &["agent", "never-existed"]);
    assert!(!output.status.success(), "an unknown tag must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("never-existed") && stderr.contains("no session tagged"),
        "the error names the tag and the workspace: {stderr}"
    );
}
