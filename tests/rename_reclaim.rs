//! Rename answers the same ownership question as start (issue #13).
//!
//! `a start` reclaims a `workspace+tag` whose holder no longer needs it --
//! `reap_verdict`, spec.md 32.1 -- but rename's conflict scan refused *any*
//! other record holding the pair, including a zombie no client could attach
//! to, list, or remove, and named no way out:
//!
//! ```text
//! $ a start --workspace W --tag T          # succeeds, reclaiming the pair
//! $ a rename s --workspace W --tag T
//! a: workspace+tag already belongs to session 57e5a6ba-...
//! ```
//!
//! The bug is the disagreement itself: one predicate must decide whether a
//! record still owns its name. So alongside the end-to-end regressions this
//! suite pins the criterion that matters -- for every holder shape (dead,
//! crashed start, mid-create, live, live-worker-only) `a start` and
//! `a rename` give the same answer and leave the same registry behind.
//!
//! Harness style follows tests/reclaim_zombie_tag.rs (direct CLI, real
//! sessions, real signals).

use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let config = state.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = self.command(args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_with_timeout(command, Duration::from_secs(30))
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}): stderr={}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "`a {}` did not print JSON ({error}): {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// Stop a live probe-created session for real, so no `sleep 300` outlives
    /// the test that started it.
    fn kill_ok(&self, id: &str) {
        let output = self
            .command(&["kill", id, "--signal", "KILL", "--grace-ms", "0"])
            .output()
            .expect("run a kill");
        assert!(
            output.status.success(),
            "`a kill {id}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A session whose workload is a plain long sleep. Killing its worker
    /// closes the PTY and takes the leader with it -- exactly how the
    /// reported zombie records were produced.
    fn start_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        self.start(workspace, tag, &["/bin/sleep", "300"])
    }

    /// A workload leader that deliberately survives its worker: it ignores
    /// the SIGHUP the closing PTY delivers, so killing the worker leaves a
    /// real orphaned process whose last durable handle is this record.
    fn start_hup_proof_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        self.start(
            workspace,
            tag,
            &["/bin/sh", "-c", "trap \"\" HUP TERM; sleep 300"],
        )
    }

    fn start(&self, workspace: &TempDir, tag: &str, command: &[&str]) -> Session {
        let mut args = vec![
            "--json",
            "start",
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
            "--",
        ];
        args.extend_from_slice(command);
        let record = self.json(&args);
        Session {
            id: record["id"].as_str().expect("session id").to_string(),
            worker_pid: record["worker_pid"].as_i64().expect("worker pid") as i32,
            workload_pid: record["workload_pid"].as_i64().expect("workload pid") as i32,
        }
    }

    fn snapshot(&self) -> Vec<Value> {
        self.json(&["--json", "list"])
            .as_array()
            .expect("snapshot array")
            .clone()
    }

    fn rows_for(&self, workspace: &TempDir, tag: &str) -> Vec<Value> {
        let workspace = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace")
            .to_str()
            .expect("UTF-8 workspace")
            .to_string();
        self.snapshot()
            .into_iter()
            .filter(|row| row["workspace"] == workspace && row["tag"] == tag)
            .collect()
    }

    fn state_dir(&self, id: &str) -> PathBuf {
        self.state.path().join("sessions").join(id)
    }

    fn state_dir_exists(&self, id: &str) -> bool {
        self.state_dir(id).exists()
    }

    fn retired_dir(&self, id: &str) -> PathBuf {
        self.state.path().join("retired-sessions").join(id)
    }

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        }
    }
}

#[derive(Clone)]
struct Session {
    id: String,
    worker_pid: i32,
    workload_pid: i32,
}

fn process_alive(pid: i32) -> bool {
    // Deliberately the production predicate, not a bare `kill(pid, 0)`:
    // that call SUCCEEDS for a zombie, so a suite using it would wait on a
    // process that is already dead and only awaiting a reaper.
    aplexer::process_alive(pid as u32)
}

fn signal_sigkill(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// Wait until the zombie-aware derived state says the session's worker is
/// gone, WITHOUT insisting the pid leave /proc first: a worker adopted by a
/// subreaper can linger as an unreaped zombie, and the production
/// predicates already count a zombie as dead
/// (`process_alive_reports_an_unreaped_zombie_as_dead`), so the derived
/// record is the post-condition that matters, not pid reaping.
fn wait_for_dead_worker(harness: &Harness, workspace: &TempDir, tag: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = harness.rows_for(workspace, tag);
        assert_eq!(rows.len(), 1, "fixture lost its record: {rows:?}");
        if rows[0]["worker_alive"] == false
            && rows[0]["phase"] == "running"
            && rows[0]["state"] == "broken"
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture never became a broken record: {}",
            rows[0]
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Reproduce the reported state: a worker SIGKILLed without recording an
/// exit, and no workload left behind. `phase` stays at whatever the worker
/// last wrote (`running`) forever.
fn make_zombie(harness: &Harness, workspace: &TempDir, tag: &str) -> Session {
    let session = harness.start_sleeper(workspace, tag);
    signal_sigkill(session.worker_pid);
    signal_sigkill(session.workload_pid);
    wait_for_dead_worker(harness, workspace, tag);
    session
}

/// A `Starting` record with no pids registered: exactly the on-disk shape
/// `start_session` publishes between writing the record and the worker
/// acquiring its lock (spec.md 32.1's spawn-to-worker-lock gap). Seeded, not
/// spawned, so the gap can be held open deterministically.
fn seed_starting_stub(harness: &Harness, workspace: &TempDir, tag: &str) -> Session {
    let paths = harness.paths();
    paths.ensure().expect("session roots");
    let id = Uuid::new_v4();
    let record = SessionRecord {
        parent_session: None,
        schema_version: SCHEMA_VERSION,
        id,
        workspace: workspace
            .path()
            .canonicalize()
            .expect("canonical workspace"),
        tag: tag.into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["/bin/sleep".into(), "300".into()],
        cwd: PathBuf::from("/tmp"),
        env: BTreeMap::new(),
        env_unset: Vec::new(),
        limits: Limits::default(),
        history_bytes: 1024,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_activity_ms: None,
        last_accessed_ms: None,
        reported_state: None,
        reported_state_at_ms: None,
        phase: Phase::Starting,
        worker_pid: None,
        workload_pid: None,
        containment_cgroup: None,
        containment_cgroup_identity: None,
        worker_cgroup: None,
        workload_cgroup: None,
        containment_empty: Some(false),
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: None,
        error: None,
    };
    fs::create_dir_all(paths.state_session(id)).expect("state session");
    fs::create_dir_all(paths.runtime_session(id)).expect("runtime session");
    atomic_write_json(&paths.record(id), &record).expect("write record");
    Session {
        id: id.to_string(),
        worker_pid: 0,
        workload_pid: 0,
    }
}

/// Overwrite one scalar field of a seeded record on disk.
fn rewrite_record(harness: &Harness, id: &str, edit: impl FnOnce(&mut Value)) {
    let record_path = harness.state_dir(id).join("session.json");
    let mut record: Value =
        serde_json::from_slice(&fs::read(&record_path).expect("read record")).expect("parse");
    edit(&mut record);
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).expect("write record");
}

/// A live worker whose workload leader is already gone. `reap_verdict`
/// retains when *either* pid is alive, so this is the shape that catches a
/// predicate looking at the leader alone. Seeded, not spawned: a real
/// aplexer worker exits shortly after its leader, so the live worker here is
/// a throwaway `sleep` that will not. No cgroup locator, so containment
/// cannot retain on its own and hide the shape.
fn seed_live_worker_dead_leader(
    harness: &Harness,
    workspace: &TempDir,
    tag: &str,
) -> (Session, std::process::Child) {
    let mut gone = Command::new("/bin/true").spawn().expect("dead leader");
    let workload_pid = gone.id() as i32;
    gone.wait().expect("reap dead leader");
    assert!(
        !process_alive(workload_pid),
        "reaped leader {workload_pid} still in /proc"
    );

    let stand_in = Command::new("/bin/sleep")
        .arg("300")
        .spawn()
        .expect("live worker stand-in");
    let worker_pid = stand_in.id() as i32;

    let session = seed_starting_stub(harness, workspace, tag);
    rewrite_record(harness, &session.id, |record| {
        record["phase"] = Value::String("running".into());
        record["worker_pid"] = Value::from(worker_pid);
        record["workload_pid"] = Value::from(workload_pid);
    });

    let rows = harness.rows_for(workspace, tag);
    assert_eq!(rows.len(), 1, "fixture lost its record: {rows:?}");
    assert_eq!(rows[0]["worker_alive"], true, "{}", rows[0]);
    assert_eq!(rows[0]["state"], "running", "{}", rows[0]);
    assert!(process_alive(worker_pid), "fixture lost its live worker");

    (
        Session {
            id: session.id,
            worker_pid,
            workload_pid,
        },
        stand_in,
    )
}

/// The holder shapes `reap_verdict` distinguishes. Each is built the same
/// way twice -- once probed by `a start`, once by `a rename` -- because the
/// criterion that matters is that the two commands answer it identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Worker and workload SIGKILLed, `phase` stuck at `running`: the
    /// reported zombie.
    Zombie,
    /// A start that crashed before its worker registered a pid, leaving a
    /// `Starting` record nobody holds a lock against.
    CrashedStart,
    /// The spawn-to-worker-lock gap with the lock held: a healthy session
    /// coming up. Neither command may take this pair (the issue #9 caveat).
    MidCreateFenced,
    /// Worker and workload leader both alive.
    Live,
    /// Worker alive, workload leader gone.
    WorkerOnly,
}

const SHAPES: &[Shape] = &[
    Shape::Zombie,
    Shape::CrashedStart,
    Shape::MidCreateFenced,
    Shape::Live,
    Shape::WorkerOnly,
];

/// What each command may conclude about the holder of the probed pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The pair changed hands.
    Taken,
    /// The holder kept it.
    Refused,
}

impl Shape {
    fn build(self, harness: &Harness, workspace: &TempDir, tag: &str) -> Holder {
        match self {
            Shape::Zombie => {
                let session = make_zombie(harness, workspace, tag);
                Holder::new(
                    session.clone(),
                    vec![session.worker_pid, session.workload_pid],
                    None,
                )
            }
            Shape::CrashedStart => {
                let mut gone = Command::new("/bin/true").spawn().expect("dead worker");
                let worker_pid = gone.id() as i32;
                gone.wait().expect("reap dead worker");
                let session = seed_starting_stub(harness, workspace, tag);
                rewrite_record(harness, &session.id, |record| {
                    record["worker_pid"] = Value::from(worker_pid);
                });
                Holder::new(session, vec![], None)
            }
            Shape::MidCreateFenced => {
                let session = seed_starting_stub(harness, workspace, tag);
                let lock_path = harness
                    .runtime
                    .path()
                    .join("sessions")
                    .join(&session.id)
                    .join("worker.lock");
                let held =
                    aplexer::FileLock::exclusive(&lock_path, true).expect("hold the worker lock");
                Holder::new(session, vec![], Some(held))
            }
            Shape::Live => {
                let session = harness.start_sleeper(workspace, tag);
                Holder::new(
                    session.clone(),
                    vec![session.worker_pid, session.workload_pid],
                    None,
                )
            }
            Shape::WorkerOnly => {
                let (session, stand_in) = seed_live_worker_dead_leader(harness, workspace, tag);
                Holder {
                    session,
                    cleanup_pids: vec![],
                    held_worker_lock: None,
                    stand_in: Some(stand_in),
                }
            }
        }
    }
}

/// A seeded holder plus everything that must stay alive (a held worker.lock,
/// a stand-in worker process) while its probe runs, and nothing left behind
/// when the arm ends.
struct Holder {
    session: Session,
    /// Live fixture processes to SIGKILL on drop (best effort; a killed
    /// process may linger as a zombie under its adopter -- the production
    /// predicates already count that as dead).
    cleanup_pids: Vec<i32>,
    /// Never read: held (not dropped) so the fenced stub's worker cannot
    /// acquire its lock and come up while the probe runs.
    #[allow(dead_code)]
    held_worker_lock: Option<aplexer::FileLock>,
    /// A process this fixture spawned that must be reaped, not just
    /// signalled, so nothing leaks.
    stand_in: Option<std::process::Child>,
}

impl Holder {
    fn new(
        session: Session,
        cleanup_pids: Vec<i32>,
        held_worker_lock: Option<aplexer::FileLock>,
    ) -> Self {
        Self {
            session,
            cleanup_pids,
            held_worker_lock,
            stand_in: None,
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if let Some(mut child) = self.stand_in.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for pid in self.cleanup_pids.drain(..) {
            signal_sigkill(pid);
        }
    }
}

/// Shared post-probe registry assertions. When the pair changed hands the
/// predecessor must be fully retired -- no second record claiming the pair,
/// no durable state, no archive -- and when it was refused the holder must
/// be exactly as it was.
fn assert_registry_matches_verdict(
    harness: &Harness,
    workspace: &TempDir,
    tag: &str,
    holder_id: &str,
    verdict: Verdict,
    new_owner: &str,
) {
    let rows = harness.rows_for(workspace, tag);
    assert_eq!(rows.len(), 1, "duplicate selectors after probe: {rows:?}");
    match verdict {
        Verdict::Taken => {
            assert_eq!(
                rows[0]["id"], new_owner,
                "the probe's session must own the pair: {}",
                rows[0]
            );
            assert!(
                !harness.state_dir_exists(holder_id),
                "predecessor's durable state survived the reclaim"
            );
            assert!(
                !harness.retired_dir(holder_id).exists(),
                "predecessor's archive survived the reclaim"
            );
        }
        Verdict::Refused => {
            assert_eq!(
                rows[0]["id"], holder_id,
                "the holder must still own the pair: {}",
                rows[0]
            );
            assert!(
                harness.state_dir_exists(holder_id),
                "a refused probe still destroyed the holder's durable state"
            );
        }
    }
}

/// A refusal must name the holder and a way out, whichever command issued
/// it -- "a session the user can't see, attach to, or remove" was the
/// reported trap, and a bare uuid in an error reproduces it.
fn assert_refusal_names_a_way_out(output: &Output, holder_id: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("workspace+tag already belongs"),
        "unexpected refusal text: {stderr}"
    );
    assert!(stderr.contains(holder_id), "{stderr}");
    assert!(
        stderr.contains("rename it or choose a different tag"),
        "refusal must suggest a next step: {stderr}"
    );
}

/// Run `a start` onto the pair `holder_id` sits on and judge the outcome the
/// way the agreement test does: success means the pair changed hands, the
/// predecessor is fully retired and the replacement is live; failure means a
/// named refusal and an untouched holder.
fn probe_start(harness: &Harness, workspace: &TempDir, tag: &str, holder_id: &str) -> Verdict {
    let output = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        tag,
        "--",
        "/bin/sleep",
        "300",
    ]);
    if output.status.success() {
        let record: Value = serde_json::from_slice(&output.stdout).expect("start JSON");
        let replacement = record["id"].as_str().expect("replacement id").to_string();
        assert_registry_matches_verdict(
            harness,
            workspace,
            tag,
            holder_id,
            Verdict::Taken,
            &replacement,
        );
        harness.kill_ok(&replacement);
        Verdict::Taken
    } else {
        assert_refusal_names_a_way_out(&output, holder_id);
        assert_registry_matches_verdict(harness, workspace, tag, holder_id, Verdict::Refused, "");
        Verdict::Refused
    }
}

/// The same probe for `a rename`: a live mover session in another workspace
/// is renamed onto the pair.
fn probe_rename(harness: &Harness, workspace: &TempDir, tag: &str, holder_id: &str) -> Verdict {
    let mover_workspace = TempDir::new().expect("mover workspace");
    let mover = harness.start_sleeper(&mover_workspace, "mover");
    let output = harness.run(&[
        "--json",
        "rename",
        &mover.id,
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        tag,
    ]);
    let verdict = if output.status.success() {
        assert_registry_matches_verdict(
            harness,
            workspace,
            tag,
            holder_id,
            Verdict::Taken,
            &mover.id,
        );
        Verdict::Taken
    } else {
        assert_refusal_names_a_way_out(&output, holder_id);
        assert_registry_matches_verdict(harness, workspace, tag, holder_id, Verdict::Refused, "");
        Verdict::Refused
    };
    let mover_rows = harness.rows_for(&mover_workspace, "mover");
    match verdict {
        Verdict::Taken => assert!(
            mover_rows.is_empty(),
            "mover kept its old pair after renaming: {mover_rows:?}"
        ),
        Verdict::Refused => assert_eq!(
            mover_rows.len(),
            1,
            "a refused rename damaged the mover: {mover_rows:?}"
        ),
    }
    harness.kill_ok(&mover.id);
    verdict
}

/// The criterion that matters (issue #13): whatever shape a holder has,
/// `a start` and `a rename` give the same answer to "does this record still
/// own its workspace+tag?" -- including when the answer is no and the pair
/// is taken. The reported bug was never that rename's answer was wrong in
/// isolation; it was that it disagreed with start's for the same holder.
#[test]
fn rename_and_start_answer_whether_a_holder_still_owns_its_pair_identically() {
    for shape in SHAPES {
        let start_verdict = {
            let harness = Harness::new();
            let workspace = TempDir::new().expect("workspace tempdir");
            let holder = shape.build(&harness, &workspace, "pair");
            probe_start(&harness, &workspace, "pair", &holder.session.id)
        };
        let rename_verdict = {
            let harness = Harness::new();
            let workspace = TempDir::new().expect("workspace tempdir");
            let holder = shape.build(&harness, &workspace, "pair");
            probe_rename(&harness, &workspace, "pair", &holder.session.id)
        };
        assert_eq!(
            start_verdict, rename_verdict,
            "start and rename disagree about the {shape:?} holder"
        );
    }
}

/// The reported bug, end to end: rename onto a pair held by a zombie used to
/// fail with "workspace+tag already belongs to session <uuid>" while
/// `a start` on the same pair succeeded. Rename must reclaim the dead pair
/// exactly as start does, retiring the corpse through the same pipeline.
#[test]
fn rename_reclaims_a_workspace_tag_held_by_a_broken_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let zombie = make_zombie(&harness, &workspace, "zt");
    let _cleanup = CleanupPids(vec![zombie.worker_pid, zombie.workload_pid]);

    let mover_workspace = TempDir::new().expect("mover workspace");
    let mover = harness.start_sleeper(&mover_workspace, "mover");
    let renamed = harness.run(&[
        "--json",
        "rename",
        &mover.id,
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        "zt",
    ]);
    assert!(
        renamed.status.success(),
        "rename refused a pair held by a broken record: stderr={}",
        String::from_utf8_lossy(&renamed.stderr)
    );

    // The mover owns the pair; the zombie is gone -- its durable state
    // retired and deleted by the same transaction a reclaiming start runs,
    // not left behind as a second record claiming the pair.
    assert_registry_matches_verdict(
        &harness,
        &workspace,
        "zt",
        &zombie.id,
        Verdict::Taken,
        &mover.id,
    );
    let mover_rows = harness.rows_for(&mover_workspace, "mover");
    assert!(
        mover_rows.is_empty(),
        "mover kept its old pair after renaming: {mover_rows:?}"
    );

    harness.kill_ok(&mover.id);
}

/// The safety property, unchanged: a live session keeps its name, and a
/// refused rename must not damage either side -- the holder keeps running
/// with its pair and its durable state, the mover keeps its own.
#[test]
fn rename_refuses_a_workspace_tag_held_by_a_live_session() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let holder = harness.start_sleeper(&workspace, "live");
    let _cleanup = CleanupPids(vec![holder.worker_pid, holder.workload_pid]);
    let mover_workspace = TempDir::new().expect("mover workspace");
    let mover = harness.start_sleeper(&mover_workspace, "mover");

    let refused = harness.run(&[
        "--json",
        "rename",
        &mover.id,
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        "live",
    ]);
    assert!(
        !refused.status.success(),
        "rename replaced a live session: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert_refusal_names_a_way_out(&refused, &holder.id);

    assert_registry_matches_verdict(
        &harness,
        &workspace,
        "live",
        &holder.id,
        Verdict::Refused,
        "",
    );
    assert!(
        process_alive(holder.worker_pid) && process_alive(holder.workload_pid),
        "a refused rename must never signal the session it was refused"
    );
    let mover_rows = harness.rows_for(&mover_workspace, "mover");
    assert_eq!(mover_rows.len(), 1, "the mover was damaged: {mover_rows:?}");

    harness.kill_ok(&mover.id);
    harness.kill_ok(&holder.id);
}

/// The other live arm: the worker is dead but the workload leader survived
/// it (HUP-proof). The pair's last durable handle is the holder's record;
/// taking it would orphan a running process, so rename must refuse exactly
/// as start does.
#[test]
fn rename_refuses_a_workspace_tag_whose_workload_is_still_alive() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let holder = harness.start_hup_proof_sleeper(&workspace, "orphan");
    let _cleanup = CleanupPids(vec![holder.worker_pid, holder.workload_pid]);
    signal_sigkill(holder.worker_pid);
    wait_for_dead_worker(&harness, &workspace, "orphan");
    assert!(
        process_alive(holder.workload_pid),
        "fixture needs a surviving workload leader"
    );

    let mover_workspace = TempDir::new().expect("mover workspace");
    let mover = harness.start_sleeper(&mover_workspace, "mover");
    let refused = harness.run(&[
        "--json",
        "rename",
        &mover.id,
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        "orphan",
    ]);
    assert!(
        !refused.status.success(),
        "rename orphaned a live workload to take its tag: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("workspace+tag already belongs"), "{stderr}");
    assert!(stderr.contains(&holder.id), "{stderr}");

    let rows = harness.rows_for(&workspace, "orphan");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], holder.id, "{}", rows[0]);
    assert!(
        harness.state_dir_exists(&holder.id),
        "a refused rename still destroyed the live workload's last handle"
    );
    assert!(
        process_alive(holder.workload_pid),
        "rename signalled a workload it was refused permission to replace"
    );

    harness.kill_ok(&mover.id);
}

/// Best-effort SIGKILL of fixture processes on drop, so a failing test
/// cannot leave `sleep 300` behind on the box.
struct CleanupPids(Vec<i32>);

impl Drop for CleanupPids {
    fn drop(&mut self) {
        for pid in self.0.drain(..) {
            signal_sigkill(pid);
        }
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    let mut child = command.spawn().expect("spawn aplexer CLI");
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let status = match rx.recv_timeout(timeout) {
        Ok(status) => status.expect("wait for aplexer CLI"),
        Err(_) => {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            panic!("`a` did not return within {timeout:?}");
        }
    };
    Output {
        status,
        stdout: out_reader.join().expect("stdout reader"),
        stderr: err_reader.join().expect("stderr reader"),
    }
}
