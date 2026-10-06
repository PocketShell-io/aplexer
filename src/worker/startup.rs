//! Bringing the worker process itself up: the startup checkpoints the
//! test hooks drive, the guard that rolls every created resource back if
//! bring-up fails, the gate that holds runtime threads until the Running
//! record is committed, and the one-shot launch environment.
//!
//! One reason to exist: a worker that fails partway through startup must
//! leave a truthful `Failed` record and nothing else behind, whichever
//! step failed.

use super::*;
use crate::persist::replace_existing_json;
use serde::Serialize;

/// Replace the session's durable record without ever creating it.
///
/// The launcher wrote the record before exec, and this worker read it under
/// the worker lock, so from here on every write is a replacement -- the same
/// invariant a running worker's `update_record` already keeps. It is also
/// the issue #22 startup fence: bring-up takes real time (workload spawn,
/// cgroup placement, history open), and a durable state deleted inside that
/// window must fail the very next write -- and with it the start -- rather
/// than be written back into existence. `atomic_write_json` recreates parent
/// directories, so a creating write here would resurrect the record, the
/// worker would commit `Running` over a session whose state tree is gone,
/// and the 500 ms vanished-record check in `serve_control_socket` would find
/// a record to keep it alive forever: a detached orphan serving a socket no
/// client can address, invisible to `a list` and unkillable.
///
/// `persist_worker_identity_once` rides along exactly as it does in
/// `atomic_write_json`, so the first registration still pins this process's
/// identity; it is a no-replace publication and fails the same way when the
/// state directory is gone.
fn replace_worker_record<T: Serialize>(path: &std::path::Path, record: &T) -> Result<()> {
    let value = serde_json::to_value(record)
        .with_context(|| format!("serialize record {}", path.display()))?;
    persist_worker_identity_once(path, &value)?;
    replace_existing_json(path, record)
}

pub(super) fn startup_checkpoint(point: &str) -> Result<()> {
    if TERMINATION_REQUESTED.load(Ordering::SeqCst) {
        bail!("worker startup cancelled by termination signal");
    }
    #[cfg(feature = "startup-test-hooks")]
    if let Ok(spec) = env::var("APLEXER_TEST_EXIT_WORKER_AT") {
        // "<checkpoint>:<exit status>". Unlike the failure hook below this
        // leaves through `process::exit`, so the worker's own StartupGuard
        // never runs and the durable record keeps whatever non-terminal
        // phase it had. That is the only way to build the two shapes the
        // API's "worker exited during startup" handling must still reject:
        // a worker gone with no terminal record at all, and one gone
        // cleanly (status 0) that never recorded an exit.
        if let Some((target, status)) = spec.split_once(':') {
            if target == point {
                std::process::exit(status.parse().unwrap_or(1));
            }
        }
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_FAIL_WORKER_STARTUP_AT").as_deref() == Ok(point) {
        bail!("injected worker startup failure at {point}");
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = point;
    Ok(())
}

pub(super) fn after_workload_spawn_checkpoint(pid: u32) -> Result<()> {
    #[cfg(feature = "startup-test-hooks")]
    if let Some(marker) = env::var_os("APLEXER_TEST_WORKER_STARTUP_MARKER") {
        atomic_write_bytes(std::path::Path::new(&marker), pid.to_string().as_bytes())
            .context("write worker startup test marker")?;
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_HANG_WORKER_STARTUP_AT").as_deref() == Ok("after_workload_spawn") {
        // Deliberately ignore TERMINATION_REQUESTED. The non-default Cargo
        // feature is the authorization boundary for this destructive hook;
        // default and release builds do not contain the hang path.
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_PAUSE_WORKER_STARTUP_AT").as_deref() == Ok("after_workload_spawn") {
        // With a resume file set, pause until the test creates it, so a test
        // can mutate the world mid-bring-up (issue #22: delete the durable
        // state under a still-starting worker) and then let startup run on
        // deterministically. Without one, the original contract holds: pause
        // until a termination request, which cancels startup below.
        if let Some(resume) = env::var_os("APLEXER_TEST_RESUME_WORKER_STARTUP_FILE") {
            let resume = std::path::PathBuf::from(resume);
            while !resume.exists() && !TERMINATION_REQUESTED.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            if TERMINATION_REQUESTED.load(Ordering::SeqCst) {
                wait_for_termination_request()?;
            }
        } else {
            wait_for_termination_request()?;
        }
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = pid;
    startup_checkpoint("after_workload_spawn")
}
/// Owns every resource created before the worker's accept loop is committed.
/// Drop is a last-resort rollback; normal error paths call `rollback` so the
/// persisted failure contains the original error rather than a generic one.
pub(super) struct StartupGuard {
    pub(super) armed: bool,
    pub(super) record_path: std::path::PathBuf,
    pub(super) runtime_session_dir: std::path::PathBuf,
    pub(super) socket_path: std::path::PathBuf,
    pub(super) failure_record: SessionRecord,
    pub(super) cgroup: Option<Cgroup>,
    pub(super) cgroup_setup_started: bool,
    pub(super) child: Option<Arc<Mutex<Option<StartupChild>>>>,
}

/// The workload leader handle the guard owns until startup commits.
#[cfg(unix)]
pub(super) type StartupChild = Child;
#[cfg(windows)]
pub(super) type StartupChild = crate::sys::windows::pty::Workload;

impl StartupGuard {
    pub(super) fn new(paths: &Paths, record: &SessionRecord) -> Self {
        Self {
            armed: true,
            record_path: paths.record(record.id),
            runtime_session_dir: paths.runtime_session(record.id),
            socket_path: paths.socket(record.id),
            failure_record: record.clone(),
            cgroup: None,
            cgroup_setup_started: false,
            child: None,
        }
    }

    pub(super) fn rollback(&mut self, error: &anyhow::Error) {
        self.cleanup(format!("{error:#}"));
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
        self.child = None;
        self.cgroup = None;
    }

    pub(super) fn cleanup(&mut self, message: String) {
        if !self.armed {
            return;
        }
        self.armed = false;

        let mut cleanup_failures = Vec::new();
        let deadline = Instant::now() + DESCENDANT_KILL_TIMEOUT;
        if let Some(cgroup) = &self.cgroup {
            if let Err(error) = cgroup.kill_all_until(deadline) {
                cleanup_failures.push(format!("kill startup cgroup: {error:#}"));
            }
        } else {
            // Windows: the session Job Object (KILL_ON_JOB_CLOSE) owns the
            // descendants; there is no process-tree walk to do here.
            #[cfg(unix)]
            {
                if let Err(error) = signal_descendants(std::process::id(), libc::SIGKILL) {
                    cleanup_failures.push(format!("kill startup descendants: {error:#}"));
                }
            }
        }

        if let Some(slot) = &self.child {
            match slot.lock() {
                Ok(mut slot) => {
                    if let Some(mut child) = slot.take() {
                        #[cfg(unix)]
                        if let Err(error) = child.kill() {
                            if error.kind() != io::ErrorKind::InvalidInput {
                                cleanup_failures
                                    .push(format!("kill startup workload leader: {error}"));
                            }
                        }
                        #[cfg(windows)]
                        if let Err(error) = child.terminate(crate::sys::windows::job::KILLED_EXIT_CODE) {
                            cleanup_failures
                                .push(format!("kill startup workload leader: {error}"));
                        }
                        if let Err(error) = child.wait() {
                            cleanup_failures.push(format!("reap startup workload leader: {error}"));
                        }
                    }
                }
                Err(_) => cleanup_failures.push("startup child lock poisoned".into()),
            }
        }

        // Once the tracked leader has been waited, every remaining process
        // is an adopted child and may safely be reaped while the domain is
        // killed again until it is observed empty.
        if let Err(error) = kill_until_empty(self.cgroup.as_ref(), deadline) {
            cleanup_failures.push(format!("prove startup containment empty: {error:#}"));
        }
        if self.cgroup_setup_started && self.cgroup.is_none() {
            cleanup_failures.push(
                "cgroup setup spawned a helper but no authoritative locator was recorded".into(),
            );
        }

        self.failure_record.phase = Phase::Failed;
        self.failure_record.containment_empty = Some(cleanup_failures.is_empty());
        self.failure_record.error = Some(if cleanup_failures.is_empty() {
            message
        } else {
            format!(
                "{message}; containment cleanup unproven: {}",
                cleanup_failures.join("; ")
            )
        });
        self.failure_record.updated_at_ms = now_ms();
        // The containment work above ran either way. But a durable record
        // deleted while bring-up ran (issue #22: the spawner died and its
        // TempDirs dropped mid-startup) must stay deleted: the startup
        // writes above refused to recreate it, and writing the `Failed`
        // record here with a creating write would resurrect the one file
        // whose absence means "this session no longer exists" -- leaving a
        // record no client can list or kill, the durable litter that pairs
        // with the orphan-worker shape. Take the runtime artifacts back out
        // (nothing can address them without the record either) and remove
        // any state files this startup managed to write, under the worker
        // lock every destroyer fences through.
        if durable_record_vanished_at(&self.record_path) {
            log_best_effort(&format!(
                "aplexer worker: durable record {} vanished during startup; \
                 not resurrecting the failed-session record",
                self.record_path.display()
            ));
            if let Some(cgroup) = self.cgroup.take() {
                cgroup.cleanup();
            }
            let _ = fs::remove_file(&self.socket_path);
            let _ = fs::remove_dir_all(&self.runtime_session_dir);
            if let Some(state_session) = self.record_path.parent() {
                match fs::remove_dir_all(state_session) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => log_best_effort(&format!(
                        "aplexer worker: remove state abandoned by vanished record: {error:#}"
                    )),
                }
            }
            return;
        }
        match atomic_write_json(&self.record_path, &self.failure_record) {
            Ok(()) if self.failure_record.containment_empty == Some(true) => {
                if let Some(cgroup) = self.cgroup.take() {
                    cgroup.cleanup();
                }
                let _ = fs::remove_file(&self.socket_path);
                let _ = fs::remove_dir_all(&self.runtime_session_dir);
            }
            Ok(()) => {}
            Err(error) => log_best_effort(&format!(
                "aplexer worker: persist startup rollback: {error:#}"
            )),
        }
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        self.cleanup("worker startup aborted before commit".into());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ThreadStart {
    Pending,
    Run,
    Abort,
}

pub(super) type ThreadStartGate = Arc<(Mutex<ThreadStart>, Condvar)>;

pub(super) fn await_thread_start(gate: &ThreadStartGate) -> bool {
    let (state, ready) = &**gate;
    let Ok(mut state) = state.lock() else {
        return false;
    };
    while *state == ThreadStart::Pending {
        let Ok(next) = ready.wait(state) else {
            return false;
        };
        state = next;
    }
    *state == ThreadStart::Run
}

pub(super) fn release_startup_threads(gate: &ThreadStartGate, decision: ThreadStart) {
    let (state, ready) = &**gate;
    if let Ok(mut state) = state.lock() {
        *state = decision;
        ready.notify_all();
    }
}

pub(super) fn load_launch_environment(
    path: &std::path::Path,
    legacy: LaunchEnvironment,
) -> Result<LaunchEnvironment> {
    match fs::read(path) {
        Ok(bytes) => {
            let bytes = SecretBytes(bytes);
            let environment = serde_json::from_slice(&bytes.0)
                .with_context(|| format!("parse private launch environment {}", path.display()))?;
            // Keeping a readable secret file after consumption is not a
            // recoverable warning. Fail startup so the transaction removes
            // the whole private runtime directory.
            fs::remove_file(path).with_context(|| {
                format!("remove consumed launch environment {}", path.display())
            })?;
            Ok(LaunchEnvironment(environment))
        }
        // Compatibility for sessions created by an older client, whose
        // launch values were stored directly in the record.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(legacy),
        Err(error) => Err(error)
            .with_context(|| format!("read private launch environment {}", path.display())),
    }
}

/// The durable record, read only once the worker lock is held.
///
/// Every destroyer of a pre-PID session (`a forget`, `a prune`, `a start`'s
/// reclaim) fences the worker through that lock and removes the durable
/// state while holding it. A record read *before* the lock could therefore
/// be stale: the destroyer unlinks the runtime dir, this worker recreates
/// it and acquires a fresh lock inode unopposed, and its first record
/// write resurrects a session that had just been forgotten. Read after the
/// lock, a record that is gone means the session no longer exists -- fail
/// the start and take the runtime dir this lock lives in back out, so the
/// refusal leaves nothing behind either.
pub(super) fn read_record_under_worker_lock(paths: &Paths, id: Uuid) -> Result<SessionRecord> {
    match read_session_record(paths, id) {
        Ok(record) => Ok(record),
        Err(error) => {
            if io_kind(&error) == Some(io::ErrorKind::NotFound) {
                let _ = fs::remove_dir_all(paths.runtime_session(id));
            }
            Err(error).with_context(|| {
                format!("session {id} has no durable record; refusing to start its worker")
            })
        }
    }
}

/// Bring the session up: consume the one-shot launch environment, publish
/// this process as the worker, bind the control socket, create the
/// containment domain and the PTY, spawn the workload, open history, and
/// start the runtime threads. Every resource created along the way is
/// owned by a `StartupGuard` until the last step commits; a failure rolls
/// them all back and persists a `Failed` record carrying the error.
pub(super) fn bring_up(
    paths: &Paths,
    mut record: SessionRecord,
    initial_size: Option<(u16, u16)>,
) -> Result<(Listener, FileIdentity, Arc<WorkerRuntime>)> {
    let id = record.id;
    let record_path = paths.record(id);
    let legacy_environment = LaunchEnvironment(std::mem::take(&mut record.env));
    record.env = session_metadata_env(&legacy_environment.0);
    let mut startup = StartupGuard::new(paths, &record);
    let setup = (|| -> Result<(Listener, FileIdentity, Arc<WorkerRuntime>)> {
        startup_checkpoint("after_worker_lock")?;
        let launch_environment_path = paths.runtime_session(id).join("launch-environment.json");
        let launch_environment =
            load_launch_environment(&launch_environment_path, legacy_environment)?;
        // Migrate a legacy record before exposing any further worker state,
        // retaining only non-secret roots needed for transcript discovery.
        record.worker_pid = Some(std::process::id());
        // Placement evidence (issue #1): the fork's pre_exec setsid() gave
        // this process a new session but left it in the ambient cgroup, so
        // whatever manager owns that cgroup can still kill this session
        // wholesale. Record where we actually are while we can still read
        // it -- after a manager-wide kill the path is gone and the failure
        // is unprovable, exactly the incident's `yolo` post-mortem problem.
        #[cfg(target_os = "linux")]
        {
            record.worker_cgroup = crate::placement::read_process_cgroup(std::process::id());
        }
        record.updated_at_ms = now_ms();
        startup.failure_record = record.clone();
        // Publish this worker's registration by replacement, and probe
        // replacement on the session's actual filesystem in the same
        // operation, before any socket or workload exists: a running worker
        // requires it (every later record write is an exchange), and the
        // startup must never create a record back into existence (issue #22
        // -- the startup window of the deleted-durable-state orphan).
        replace_worker_record(&record_path, &record)
            .context("session state filesystem must support RENAME_EXCHANGE")?;
        startup_checkpoint("after_worker_record")?;

        // On Windows paths.socket(id) is the named-pipe name
        // (sys::windows::ipc::pipe_name): no stale node to remove and no
        // inode identity to track. FIRST_PIPE_INSTANCE makes a squatted name
        // fail the bind with AddrInUse instead of being adopted.
        let socket_path = paths.socket(id);
        #[cfg(unix)]
        {
            if socket_path.exists() {
                fs::remove_file(&socket_path).context("remove stale control socket")?;
            }
        }
        let listener = Listener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        #[cfg(unix)]
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        #[cfg(unix)]
        let socket_identity = trusted_socket_identity(&socket_path)?;
        #[cfg(windows)]
        let socket_identity: FileIdentity = (0, 0);
        startup_checkpoint("after_control_socket")?;

        let requested_size = initial_size.unwrap_or((24, 80));
        let (rows, cols) = screen::validate_worker_size(requested_size.0, requested_size.1)?;
        // A capped launch resolves its placement decision, trusted helpers,
        // and kernel-side identity without spawning anything. The scope
        // itself now comes into being around the workload in the same
        // systemd transaction (`systemd-run` places the workload as the
        // scope's initial process), so the containment locator is only
        // known after the spawn below -- the durable record is written
        // immediately after, before any injected or real post-spawn failure.
        #[cfg(unix)]
        let (child, cgroup, workload_pid, master_read, master_write) = {
            let cgroup_plan =
                ScopePlan::prepare(id, &record.limits).context("resolve workload scope")?;
            startup_checkpoint("after_cgroup")?;
            let (master_read, slave) = open_pty(rows, cols)?;
            let master_write = master_read.try_clone()?;
            let child_result = spawn_workload(
                &record,
                &launch_environment.0,
                master_read.as_raw_fd(),
                slave,
                cgroup_plan,
                || {
                    startup.cgroup_setup_started = true;
                },
            );
            // Launch values are one-shot: overwrite them as soon as spawn has
            // either succeeded or failed, never retaining them in the accept
            // loop or its background threads.
            drop(launch_environment);
            let (child, cgroup, workload_pid) = child_result?;
            (child, cgroup, workload_pid, master_read, master_write)
        };
        // Windows: ConPTY master, one named Job Object per session
        // (`aplexer-<uuid>`, KILL_ON_JOB_CLOSE) that the workload is born
        // inside, wrapped in the job-backed `Cgroup` shim so the runtime's
        // containment code is shared with Linux.
        #[cfg(windows)]
        let (child, cgroup, workload_pid, master_read, master_write) = {
            use crate::sys::windows::{job, pty::PtyMaster, signal};
            startup_checkpoint("after_cgroup")?;
            let pty = PtyMaster::open(rows, cols).context("create pseudoconsole")?;
            let limits = job::JobLimits {
                memory_bytes: record.limits.memory_bytes,
                pids: record.limits.pids,
                cpu_quota_us: record.limits.cpu_quota_us,
                cpu_period_us: record.limits.cpu_period_us,
            };
            let session_job = job::Job::create(id, &limits).context("create session job")?;
            job::install_session_job(session_job.clone());
            let cgroup = Cgroup::new(session_job.clone());
            // From here the job exists: let a failed startup kill it.
            startup.cgroup = Some(cgroup.clone());
            let input = pty.writer().context("duplicate PTY input handle")?;
            signal::install_input_writer(move |bytes| {
                let mut input = &input;
                input.write_all(bytes)?;
                input.flush()
            });
            let child_result = spawn_workload(
                &record,
                &launch_environment.0,
                &pty,
                Some(session_job.as_raw_handle()),
            );
            drop(launch_environment);
            let (child, workload_pid) = child_result?;
            let master_write = PtyWrite::new(pty.clone()).context("open PTY input")?;
            (child, Some(cgroup), workload_pid, pty, master_write)
        };
        startup.cgroup = cgroup.clone();
        #[cfg(unix)]
        let pid = child.id();
        #[cfg(windows)]
        let pid = child.pid();
        // Claim the leader before any code path can wait on it. The reaper
        // thread does not exist yet, but the claim is what documents (and
        // enforces) that `run_child_waiter` owns this pid's exit status.
        // For a capped session this is the `systemd-run` wrapper; the
        // wrapper stays the workload's parent and exits right after it.
        own_child_pid(pid);
        let child_slot = Arc::new(Mutex::new(Some(child)));
        startup.child = Some(Arc::clone(&child_slot));
        record.workload_pid = Some(workload_pid);
        // Windows records no cgroup locator: the job name derives from the id.
        #[cfg(unix)]
        if cgroup.is_some() {
            record.containment_cgroup =
                cgroup.as_ref().map(|cgroup| cgroup.locator().to_path_buf());
            record.containment_cgroup_identity =
                cgroup.as_ref().map(|cgroup| cgroup.identity().clone());
        }
        // Launch-time cgroup validation (issue #1): read where the workload
        // leader actually landed and, for a limited session, check that
        // against the scope systemd placed it in. The leader pid comes
        // straight from the scope's membership, so a mismatch would mean
        // systemd placed something else; say so in worker.log instead of
        // silently trusting the persisted locator.
        #[cfg(target_os = "linux")]
        {
            record.workload_cgroup = crate::placement::read_process_cgroup(workload_pid);
            if let (Some(cgroup), Some(actual)) =
                (cgroup.as_ref(), record.workload_cgroup.as_deref())
            {
                let expected = cgroup.proc_path();
                if actual != expected {
                    log_best_effort(&format!(
                        "warning: workload pid {workload_pid} is in cgroup {actual}, not the recorded \
                         containment scope {expected}; resource limits may not apply to the \
                         workload's real location"
                    ));
                }
            }
        }
        startup.failure_record = record.clone();
        // Publish the leader and cgroup locator before any injected or real
        // post-spawn failure. The launcher must never have to infer a
        // containment domain from an unpersisted in-memory PID.
        replace_worker_record(&record_path, &record).context("publish workload registration")?;
        after_workload_spawn_checkpoint(pid)?;

        startup_checkpoint("before_history_open")?;
        validate_existing_history_node(&record.history_path)?;
        let history = History::open(record.history_path.clone(), record.history_bytes)?;
        startup_checkpoint("before_output_hub")?;
        let output = OutputHub::new(history, rows, cols, paths.screen_txt(id))?;
        record.phase = Phase::Running;
        record.updated_at_ms = now_ms();
        record.error = None;
        startup.failure_record = record.clone();
        let runtime = Arc::new(WorkerRuntime {
            id,
            paths: paths.clone(),
            record_path: record_path.clone(),
            runtime_session_dir: paths.runtime_session(id),
            socket_path,
            record: Mutex::new(record.clone()),
            pty_write: Mutex::new(Some(Arc::new(master_write))),
            workload: Mutex::new(WorkloadState {
                running: true,
                pgid: pid as i32,
            }),
            terminal: Mutex::new(TerminalState {
                rows,
                cols,
                clients: HashMap::new(),
                next_client_id: 1,
            }),
            cgroup: Mutex::new(cgroup),
            kill_gate: Mutex::new(()),
            output,
            record_persistence_error: Mutex::new(None),
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: AtomicU64::new(0),
        });
        start_worker_threads(
            Arc::clone(&runtime),
            master_read,
            Arc::clone(&child_slot),
            || {
                replace_worker_record(&record_path, &record).context("commit running record")?;
                startup_checkpoint("after_running_record")
            },
        )?;
        Ok((listener, socket_identity, runtime))
    })();
    match setup {
        Ok(value) => {
            startup.disarm();
            Ok(value)
        }
        Err(error) => {
            startup.rollback(&error);
            Err(error)
        }
    }
}
