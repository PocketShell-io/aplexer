//! The workload's life story: the events the runtime threads feed in, and
//! the one thread that turns them into finalization.
//!
//! One reason to exist: exactly one place decides what a session's death
//! means. PtyEof/ChildExit/output exhaustion feed the same loop; the loop
//! kills the containment domain, writes the terminal record (or keeps it as
//! evidence when finalization cannot be proven), drains attached clients,
//! and exits the process -- in that order, under the kill gate. A session
//! whose files are deleted out from under it (issue #22) dies the same way:
//! the loop's bounded wait wakes to find the runtime session dir and the
//! durable record gone, and everything after is the ordinary death path.

use super::*;

pub(super) enum LifeEvent {
    PtyEof,
    PtyError(String),
    WaiterError(String),
    ChildExit {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Kill, reap, and inspect a containment domain until it is observed empty
/// or `deadline` passes. Repeating the kill closes the fork-vs-scan window;
/// reaping between passes keeps zombies from reading as live members. Only
/// an observed empty domain is proof, so a deadline is an error, never a
/// quiet success.
///
/// The caller must already have waited on every child it owns itself (the
/// workload leader in particular): `reap_adopted_children` names `-1`, and
/// would otherwise hand that leader's exit status to a thread that
/// discards it.
pub(super) fn kill_until_empty(cgroup: Option<&Cgroup>, deadline: Instant) -> Result<()> {
    loop {
        match cgroup {
            Some(cgroup) => cgroup
                .kill_all_until(deadline)
                .context("kill containment cgroup")?,
            None => {
                signal_descendants(std::process::id(), libc::SIGKILL)
                    .context("kill contained descendants")?;
            }
        }
        reap_adopted_children().context("reap contained descendants")?;
        let populated = match cgroup {
            Some(cgroup) => cgroup.populated()?,
            None => !descendant_pids(std::process::id())?.is_empty(),
        };
        if !populated {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out proving containment empty");
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

/// A waiter failure means nobody owns the tracked Child any longer. Before
/// the subreaper is allowed to exit, repeatedly kill, reap, and inspect its
/// complete containment domain (`kill_until_empty`).
pub(super) fn cleanup_after_lifecycle_failure(runtime: &WorkerRuntime) -> Result<()> {
    let _serialized = lock(&runtime.kill_gate)?;
    let cgroup = lock(&runtime.cgroup)?.clone();
    kill_until_empty(cgroup.as_ref(), Instant::now() + DESCENDANT_KILL_TIMEOUT)
        .context("failed lifecycle containment")?;
    if let Ok(mut state) = runtime.workload.lock() {
        state.running = false;
    }
    Ok(())
}

pub(super) enum LifecycleWake {
    Event(LifeEvent),
    CleanupPoll,
    /// The bounded wait behind the workload-running branch of
    /// `wait_for_lifecycle_wake` timed out: time to check that this
    /// worker's session still exists on disk (issue #22).
    RuntimeDirPoll,
    Disconnected,
}

/// Block while the tracked child is still running (or while a post-exit PTY
/// is still held open by a descendant). Two cadences:
///
/// * post-exit (`cleanup_polling`), the 25 ms containment scans are needed:
///   at that point an adopted descendant can exit without producing another
///   LifeEvent.
/// * while the workload runs, the loop used to block in `recv` with no timer
///   at all -- which is exactly why a worker orphaned by a dropped `TempDir`
///   (issue #22) never noticed: no event ever comes, so nothing re-checked
///   the filesystem. This branch now wakes on `RUNTIME_DIR_POLL_INTERVAL`
///   instead, and the loop turns each wake into a `session_vanished` check.
///   One timed wait per second is not a busy loop, and it is what keeps the
///   idle-worker budget in `tests/worker_idle_wakeups.rs` honest.
pub(super) fn wait_for_lifecycle_wake(
    rx: &mpsc::Receiver<LifeEvent>,
    cleanup_polling: bool,
) -> LifecycleWake {
    let timeout = if cleanup_polling {
        DESCENDANT_POLL_INTERVAL
    } else {
        RUNTIME_DIR_POLL_INTERVAL
    };
    match rx.recv_timeout(timeout) {
        Ok(event) => LifecycleWake::Event(event),
        Err(mpsc::RecvTimeoutError::Timeout) if cleanup_polling => LifecycleWake::CleanupPoll,
        Err(mpsc::RecvTimeoutError::Timeout) => LifecycleWake::RuntimeDirPoll,
        // Once both producer threads have ended the channel remains
        // permanently disconnected, so recv_timeout returns immediately.
        // Retain the intended cleanup cadence instead of turning that state
        // into a busy loop while adopted descendants drain -- the loop's
        // only normal exit is an observed-empty domain, not a disconnected
        // channel. (Before the workload has exited a disconnect is a real
        // fault: the caller ends the loop on that wake.)
        Err(mpsc::RecvTimeoutError::Disconnected) if cleanup_polling => {
            thread::sleep(DESCENDANT_POLL_INTERVAL);
            LifecycleWake::CleanupPoll
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => LifecycleWake::Disconnected,
    }
}

/// Remove a cleanly finished session's durable state, after fencing every
/// later durable write (`WorkerRuntime::mark_finalized`): the periodic
/// flush thread and any in-flight connection keep running through the
/// connection-drain window below, and `atomic_write_*` recreates parent
/// directories, so an unfenced write would resurrect the directory with a
/// `phase: exiting` record and a dead worker pid.
///
/// A removal that could not happen (a read-only state dir, a vanished
/// mount) is reported rather than exited on silently. Nothing is lost when
/// it fails: the record left behind still says the worker was running, its
/// pid is about to be gone, and `a prune` reaps that shape -- but the
/// operator should be able to see why a session they ended is still listed.
fn remove_finished_state(runtime: &WorkerRuntime) {
    if let Err(error) = runtime.mark_finalized() {
        log_best_effort(&format!(
            "aplexer worker: fence writes before removing finished session: {error:#}"
        ));
        return;
    }
    // Leave the finished-tombstone before the state dir goes: a `a kill`
    // that resolves its target after this point must learn "already
    // finished" instead of "no matching session" (issue #2665). The
    // workload ended before finalization either way -- by exiting or by an
    // accepted kill -- so Finished is the honest cause from here.
    if let Ok(record) = runtime.record() {
        crate::retired::write_finished_tombstone(
            &runtime.paths,
            runtime.id,
            &record.workspace,
            &record.tag,
            crate::retired::TombstoneCause::Finished,
        );
    }
    if let Err(error) = fs::remove_dir_all(runtime.paths.state_session(runtime.id)) {
        log_best_effort(&format!(
            "aplexer worker: remove finished session {} state: {error:#}",
            runtime.id
        ));
    }
}

/// The observation the lifecycle loop carries between wakes: whether the
/// PTY reached EOF, the workload leader's exit status once seen, the first
/// fatal error, and whether the containment domain was observed empty (the
/// loop's normal exit condition).
struct LoopState {
    pty_eof: bool,
    child_exit: Option<(Option<i32>, Option<i32>)>,
    fatal: Option<String>,
    containment_empty: bool,
}

impl LoopState {
    fn new() -> Self {
        Self {
            pty_eof: false,
            child_exit: None,
            fatal: None,
            containment_empty: false,
        }
    }

    fn add_fatal(&mut self, message: String) {
        self.fatal = Some(match self.fatal.take() {
            Some(existing) => format!("{existing}; {message}"),
            None => message,
        });
    }
}

/// Drop the worker's half of the PTY so writers see EOF.
fn close_pty(runtime: &WorkerRuntime) {
    if let Ok(mut pty) = runtime.pty_write.lock() {
        *pty = None;
    }
}

/// Folds one LifeEvent into the loop state. Returns true when the loop
/// must stop immediately: a waiter failure means nobody owns the tracked
/// Child any longer, so descendant polling below cannot help.
fn handle_life_event(runtime: &WorkerRuntime, state: &mut LoopState, event: LifeEvent) -> bool {
    match event {
        LifeEvent::PtyEof => {
            state.pty_eof = true;
            close_pty(runtime);
            false
        }
        LifeEvent::PtyError(message) => {
            state.pty_eof = true;
            state.fatal = Some(message.clone());
            close_pty(runtime);
            runtime.output.fail_subscribers(message);
            false
        }
        LifeEvent::WaiterError(message) => {
            state.fatal = Some(message.clone());
            close_pty(runtime);
            runtime.output.fail_subscribers(message);
            true
        }
        LifeEvent::ChildExit { code, signal } => {
            state.child_exit = Some((code, signal));
            // Natural exits (Ctrl-D, `exit`, a command that ran to
            // completion, an externally signalled workload) get their
            // exiting transition here, at the leader's death. An
            // accepted `a kill` writes the same phase earlier, at
            // acceptance, before teardown starts (issue #18) -- this
            // write is then a no-op refresh that only stamps
            // `updated_at_ms`.
            let _ = runtime.update_record(|r| r.phase = Phase::Exiting);
            false
        }
    }
}

/// After the leader has exited: reap adopted descendants and re-check the
/// containment domain each wake. Returns true when the domain is observed
/// empty after PTY EOF -- the loop's only normal exit.
fn poll_post_exit_descendants(runtime: &WorkerRuntime, state: &mut LoopState) -> bool {
    if state.child_exit.is_none() {
        return false;
    }
    if let Err(error) = reap_adopted_children() {
        state
            .fatal
            .get_or_insert_with(|| format!("reap descendants: {error:#}"));
    }
    match runtime.workload_populated() {
        Ok(populated) => {
            if let Ok(mut workload) = runtime.workload.lock() {
                workload.running = populated;
            }
            state.pty_eof && !populated
        }
        Err(error) => {
            // Fail closed: never finalize evidence while we cannot
            // establish that the containment domain is empty.
            state
                .fatal
                .get_or_insert_with(|| format!("inspect descendants: {error:#}"));
            false
        }
    }
}

/// Whether this worker's session has vanished from the filesystem in the
/// orphan shape of issue #22: the runtime session dir (socket, worker lock)
/// AND the durable record are both gone.
///
/// Each absence alone already has its own answer. A missing runtime dir
/// alone is the socket recovery path's cue to recreate it
/// (`recover_control_socket`): cleanup software passing through the runtime
/// dir must not end a session its durable record still lists. A missing
/// record alone is the accept loop's self-reap (issue #21). But both gone
/// together -- an integration test's `TempDir` dropped, `a forget`, a
/// superseding start -- means no client can ever list, attach, or kill this
/// session again, and while the workload runs this loop used to block in
/// `recv` waiting for an exit that might never come. NotFound-only, like
/// `durable_record_vanished`: a busy mount or any other read failure stays
/// ambiguous, and an ambiguous filesystem never reaps a live session.
pub(super) fn session_vanished(runtime: &WorkerRuntime) -> bool {
    matches!(
        fs::symlink_metadata(&runtime.runtime_session_dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ) && control_socket::durable_record_vanished(runtime)
}

/// The event loop: fold LifeEvents and descendant polls into LoopState
/// until the containment domain is observed empty, a waiter failure ends
/// ownership, the channel disconnects, or the session vanishes from the
/// filesystem (issue #22).
fn observe_lifecycle(runtime: &WorkerRuntime, rx: &mpsc::Receiver<LifeEvent>) -> LoopState {
    let mut state = LoopState::new();
    loop {
        let cleanup_polling = state.child_exit.is_some() && state.pty_eof;
        match wait_for_lifecycle_wake(rx, cleanup_polling) {
            LifecycleWake::Event(event) => {
                if handle_life_event(runtime, &mut state, event) {
                    break;
                }
            }
            LifecycleWake::CleanupPoll => {}
            LifecycleWake::RuntimeDirPoll => {
                if session_vanished(runtime) {
                    log_best_effort(&format!(
                        "aplexer worker: session dir {} and durable record are gone; \
                         no client can list, attach, or kill this session any more -- \
                         finalizing and exiting",
                        runtime.runtime_session_dir.display()
                    ));
                    state.fatal = Some(format!(
                        "session dir {} vanished",
                        runtime.runtime_session_dir.display()
                    ));
                    break;
                }
            }
            LifecycleWake::Disconnected => {
                state.fatal = Some("workload lifecycle channel disconnected".into());
                break;
            }
        }
        if poll_post_exit_descendants(runtime, &mut state) {
            state.containment_empty = true;
            break;
        }
    }
    state
}

/// Run the containment cleanup a failed finalization needs, folding a
/// still-unproven domain into `fatal` so the record carries the reason.
fn recover_unproven_containment(runtime: &WorkerRuntime, state: &mut LoopState) {
    match cleanup_after_lifecycle_failure(runtime) {
        Ok(()) => state.containment_empty = true,
        Err(error) => state.add_fatal(format!("containment cleanup unproven: {error:#}")),
    }
}

/// The durable terminal-record path: flush final history, then write the
/// `Exited`/`Failed` record, retrying forever rather than exiting with a
/// record that still claims this worker/workload is running.
fn record_final_state(runtime: &WorkerRuntime, exit: &ExitInfo, state: &mut LoopState) {
    if let Err(history_error) = runtime.output.flush_history(true) {
        state.add_fatal(format!("persist final history: {history_error:#}"));
    }
    let error = state.fatal.clone();
    let mut record_retry = HISTORY_RETRY_INITIAL;
    loop {
        // External retirement is deliberate; there is no stale record to repair.
        if control_socket::durable_record_vanished(runtime) {
            return;
        }
        let final_error = error.clone();
        match runtime.update_record(|r| {
            r.phase = if final_error.is_some() {
                Phase::Failed
            } else {
                Phase::Exited
            };
            r.containment_empty = Some(state.containment_empty);
            r.exit = Some(exit.clone());
            r.error = final_error;
        }) {
            Ok(_) => break,
            Err(persist_error) => {
                // Never exit with a durable record that still claims this
                // worker/workload is running. Keep the control socket alive
                // so Status can expose `record_persistence_error` while the
                // lifecycle retries.
                log_best_effort(&format!(
                    "aplexer worker: persist final session state: {persist_error:#}; retrying in {}ms",
                    record_retry.as_millis()
                ));
                thread::sleep(record_retry);
                record_retry = record_retry.saturating_mul(2).min(HISTORY_RETRY_MAX);
            }
        }
    }
}

/// The containment domain never drained cleanly: fail the remaining
/// subscribers, then retain the worker as the subreaper boundary, along
/// with its socket, cgroup handle, and runtime evidence. A later `a kill`
/// can retry; this monitor finalizes only after it independently observes
/// the resulting domain empty and durably records that proof. Returns the
/// cgroup handle to clean up, if proof (and its takeover) succeeded.
fn await_late_containment_proof(runtime: &WorkerRuntime, state: &LoopState) -> Option<Cgroup> {
    runtime
        .output
        // Cloned so the killed-session removal below can still ask
        // whether finalization failed; this path runs only when it did.
        .fail_subscribers(
            state
                .fatal
                .clone()
                .unwrap_or_else(|| "containment cleanup was not proven".into()),
        );
    loop {
        if let Err(error) = reap_adopted_children() {
            log_best_effort(&format!(
                "aplexer worker: reap after lifecycle failure: {error:#}"
            ));
        }
        match runtime.workload_populated() {
            Ok(false) => {
                let persisted = match control_socket::durable_record_vanished(runtime) {
                    true => Ok(()),
                    false => runtime
                        .update_record(|record| record.containment_empty = Some(true))
                        .map(|_| ()),
                };
                if let Err(error) = persisted {
                    log_best_effort(&format!(
                        "aplexer worker: persist delayed containment proof: {error:#}"
                    ));
                } else {
                    return runtime
                        .cgroup
                        .lock()
                        .ok()
                        .and_then(|mut cgroup| cgroup.take());
                }
            }
            Ok(true) => {}
            Err(error) => log_best_effort(&format!(
                "aplexer worker: inspect failed lifecycle containment: {error:#}"
            )),
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

/// Decide whether the session's durable record should survive the exit.
///
/// A clean, proven-empty finish leaves no record: a workload that
/// returned (zero or non-zero), Ctrl-D at a shell, a signalled workload,
/// and `a kill` are all the same path. A session that is over is gone
/// from `a list` the moment it is over -- not an `exited` tombstone that
/// sits there until somebody runs `a prune` -- and the post-mortem
/// writes that tombstone needed would be fsync-and-delete waste anyway
/// (benchmark PLAN P0.2).
///
/// Keep the durable `finish` path only when:
///
///  * something failed (`fatal`: a history flush or record persist error,
///    a PTY/waiter error) -- the record carries the reason, and nothing
///    else would report it;
///  * containment is not proven empty -- the record is the only remaining
///    handle on a domain that may still hold live processes, and dropping
///    it would strand them (`reap_verdict` / `a prune`'s bar);
///  * the workload was OOM-killed -- `oom_killed` is a diagnosis the
///    kernel made and the exit status alone does not carry, so it would
///    be unrecoverable rather than merely unrecorded;
///  * the operator asked for post-mortem records with `keep_exited = true`
///    in the config, which restores the old `exited`-until-pruned rows.
///
/// Read here rather than at worker startup so it costs nothing on the
/// start path and so editing the config takes effect for sessions that
/// are already running.
fn finalize_session(runtime: &WorkerRuntime, state: &mut LoopState) {
    let (code, signal) = state.child_exit.unwrap_or((None, None));
    let (oom, mut cg) = match runtime.cgroup.lock() {
        Ok(mut g) => {
            let oom = g.as_ref().map(Cgroup::oom_killed).unwrap_or(false);
            let cg = if state.containment_empty {
                g.take()
            } else {
                None
            };
            (oom, cg)
        }
        Err(_) => (false, None),
    };
    let exit = ExitInfo {
        code,
        signal,
        oom_killed: oom,
        exited_at_ms: now_ms(),
    };
    let keep_exited = crate::config_keep_exited(&runtime.paths);
    let will_remove = state.fatal.is_none() && state.containment_empty && !oom && !keep_exited;
    if will_remove {
        runtime.output.finish_killed(exit.clone());
        if let Some(c) = cg.take() {
            c.cleanup();
        }
        remove_finished_state(runtime);
        return;
    }
    record_final_state(runtime, &exit, state);
    // The terminal record carries the crash, but only until the `a prune`
    // (or the list sweep) that reaps finished sessions takes it away, and a
    // human may not be looking by then. Record it as an ack-gated warning
    // too (crate::warnings) -- exactly what `warning_for_record` would
    // derive from this same final record, so the worker's write and the
    // clients' query-time sweep can never disagree about what a crash is.
    // Best-effort: a failed warning write must not stop finalization.
    if oom || state.fatal.is_some() {
        match runtime.record() {
            Ok(record) => {
                if let Err(error) = crate::warnings::record_warning(&runtime.paths, &record) {
                    log_best_effort(&format!("aplexer worker: record crash warning: {error:#}"));
                }
            }
            Err(error) => {
                log_best_effort(&format!("aplexer worker: record crash warning: {error:#}"))
            }
        }
    }
    if !state.containment_empty {
        cg = await_late_containment_proof(runtime, state);
    }
    runtime.output.finish(exit.clone());
    if let Some(cg) = cg {
        cg.cleanup();
    }
    // Failed and OOM sessions keep the terminal record, as does an
    // explicit `keep_exited = true`. A clean finish whose containment
    // proof arrived late (the recovery loop above) still removes:
    // SIGTERM-to-worker, a descendant that outlived the leader, and
    // Ctrl-D are the same "gone from `a list`" outcome as the fast
    // path. Any remaining `fatal` keeps the evidence.
    if state.fatal.is_none() && !oom && !keep_exited {
        remove_finished_state(runtime);
    }
}

/// The workload is gone and the final record/history are persisted;
/// a daemonless design must not leave a worker process (plus its
/// socket and runtime dir) behind for every session that ever ran.
/// Unlink the socket first so new clients fail fast and fall back to
/// the persisted record/history, then give in-flight connections
/// (the `kill` response, attach Exit events) a bounded window to
/// drain before exiting the process. Drained at the 5 ms kill cadence
/// (benchmark PLAN P0.2), not the 25 ms lifecycle one.
fn shutdown_worker(runtime: &WorkerRuntime) {
    let _ = fs::remove_file(&runtime.socket_path);
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    while runtime.active_connections.load(Ordering::SeqCst) > 0 && Instant::now() < drain_deadline {
        thread::sleep(KILL_POLL_INTERVAL);
    }
    let _ = fs::remove_dir_all(&runtime.runtime_session_dir);
    std::process::exit(0);
}

pub(super) fn run_lifecycle(runtime: Arc<WorkerRuntime>, rx: mpsc::Receiver<LifeEvent>) {
    let mut state = observe_lifecycle(&runtime, &rx);
    if !state.containment_empty && state.fatal.is_some() {
        recover_unproven_containment(&runtime, &mut state);
    }
    finalize_session(&runtime, &mut state);
    shutdown_worker(&runtime);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub(super) fn lifecycle_wait_blocks_until_an_event_before_cleanup_is_needed() {
        let (life_tx, life_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let woke_for_event = matches!(
                wait_for_lifecycle_wake(&life_rx, false),
                LifecycleWake::Event(LifeEvent::PtyEof)
            );
            done_tx.send(woke_for_event).unwrap();
        });
        started_rx.recv().unwrap();

        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(75)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        life_tx.send(LifeEvent::PtyEof).unwrap();
        assert!(done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("lifecycle event did not wake waiter"));
        waiter.join().unwrap();
    }

    /// While the workload runs the wait is bounded, not indefinite: with no
    /// event coming, the loop must still wake to run the vanished-session
    /// check (issue #22). This is the property whose absence let an orphaned
    /// worker block in `recv` forever.
    #[test]
    pub(super) fn workload_running_wait_wakes_for_the_vanished_session_check() {
        let (_life_tx, life_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let woke = matches!(
                wait_for_lifecycle_wake(&life_rx, false),
                LifecycleWake::RuntimeDirPoll
            );
            done_tx.send(woke).unwrap();
        });
        started_rx.recv().unwrap();

        let woke_for_dir_check = done_rx
            .recv_timeout(RUNTIME_DIR_POLL_INTERVAL + Duration::from_secs(2))
            .expect("workload-running wait never returned");
        assert!(
            woke_for_dir_check,
            "wait woke as something other than the dir poll"
        );
        waiter.join().unwrap();
    }
}
