//! The worker's shared runtime state: the record, the PTY write side, the
//! workload and terminal registries, and the operations every connection
//! performs against them.
//!
//! One reason to exist: every mutation of shared worker state -- a record
//! write, a PTY resize, an input send, a kill -- goes through one struct
//! whose methods say which lock each one holds and why, so a new caller
//! cannot invent a fourth lock order.

use super::*;

#[derive(Debug)]
pub(super) struct WorkloadState {
    pub(super) running: bool,
    pub(super) pgid: i32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AttachedClient {
    /// The client's own terminal geometry (already normalized by
    /// `validate_worker_size`), or `None` for an attach that did not report
    /// one -- a `want_screen` snapshot is rendered at the shared size
    /// regardless, and a geometry-less client must not be able to pull the
    /// shared size around.
    pub(super) geometry: Option<(u16, u16)>,
}

/// The one PTY has one size, even when several clients are attached, so the
/// size has to be one every attached client can show at once: the smallest
/// height and the smallest width any of them has reported.
///
/// tmux's default (`window-size=latest`) instead hands the PTY to whichever
/// client was most recently active, and that is what made a session attached
/// from two devices resize back and forth forever -- every keystroke on one
/// device moved the shared PTY to *its* geometry and the workload repainted
/// at the new size, so typing on either device reflowed the other one's
/// screen. The common denominator cannot do that: input does not enter the
/// arithmetic at all, and the size only moves when the smallest client
/// actually resizes.
///
/// Keep the client registry and the applied size behind the same mutex so the
/// size a client is shown and the size the PTY has cannot disagree.
pub(super) struct TerminalState {
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) clients: HashMap<u64, AttachedClient>,
    pub(super) next_client_id: u64,
}

impl TerminalState {
    /// The geometry that fits every attached client at once: the smallest
    /// row count and the smallest column count reported by any of them.
    ///
    /// Componentwise rather than "the single smallest client" (tmux's
    /// `window-size=smallest`, which picks one client by area): a 10x200
    /// phone held in portrait and a 50x5 side-by-side split are *both* fully
    /// shown only by the componentwise minimum, and neither one's own
    /// geometry would do. `None` while no client has reported a geometry,
    /// which leaves the PTY at the size it has -- what tmux does for a window
    /// nothing is attached to.
    fn common_size(&self) -> Option<(u16, u16)> {
        self.clients
            .values()
            .filter_map(|client| client.geometry)
            .reduce(|(rows, cols), (other_rows, other_cols)| {
                (rows.min(other_rows), cols.min(other_cols))
            })
    }
}

pub(super) struct WorkerRuntime {
    /// The session this worker serves. Immutable for the worker's life, so
    /// no caller needs to clone the whole record out of its mutex to
    /// learn it (every connection used to).
    pub(super) id: Uuid,
    pub(super) paths: Paths,
    pub(super) record_path: std::path::PathBuf,
    pub(super) runtime_session_dir: std::path::PathBuf,
    pub(super) socket_path: std::path::PathBuf,
    pub(super) record: Mutex<SessionRecord>,
    /// The PTY master's write side. `None` once the lifecycle sees EOF.
    /// Held as an `Arc` so `send` can clone the handle and write *outside*
    /// the mutex: a PTY write blocks whenever the tty input queue is full
    /// behind a stopped foreground job, and holding the lock across it
    /// used to block `Status` (foreground_command needs the fd), every
    /// resize, and the lifecycle's PtyEof handler behind one wedged client.
    pub(super) pty_write: Mutex<Option<Arc<File>>>,
    pub(super) workload: Mutex<WorkloadState>,
    pub(super) terminal: Mutex<TerminalState>,
    pub(super) cgroup: Mutex<Option<Cgroup>>,
    pub(super) kill_gate: Mutex<()>,
    pub(super) output: OutputHub,
    /// Most recent failure to durably write the session record. Kept live so
    /// status remains truthful while the lifecycle retries final evidence.
    pub(super) record_persistence_error: Mutex<Option<String>>,
    /// Connections currently being served; the lifecycle thread drains this
    /// (with a timeout) before exiting the worker so in-flight responses
    /// (e.g. the reply to the `kill` that ended the workload) are not lost.
    pub(super) active_connections: Arc<AtomicUsize>,
    /// Last PTY-output timestamp (ms since epoch), updated on every PTY read
    /// with a single relaxed atomic store -- no lock, no I/O -- so this can
    /// sit directly in the hot PTY-reader loop without reintroducing the
    /// per-read write amplification the history-persistence debounce fix
    /// (see HISTORY_FLUSH_INTERVAL) already solved once. The periodic flush
    /// thread piggybacks on that same tick to persist this into
    /// `SessionRecord::last_activity_ms`, and only when it actually changed.
    pub(super) last_activity_ms: AtomicU64,
}

impl WorkerRuntime {
    pub(super) fn record(&self) -> Result<SessionRecord> {
        Ok(lock(&self.record)?.clone())
    }
    pub(super) fn update_record<F>(&self, update: F) -> Result<SessionRecord>
    where
        F: FnOnce(&mut SessionRecord),
    {
        let published = {
            let mut record = lock(&self.record)?;
            // Checked under the record lock (see `OutputHub::finalized`): a
            // write that got here first has already landed before the removal,
            // and one that gets here later must not recreate the state dir.
            if self.output.finalized() {
                bail!(
                    "session {} is finalized; its durable record has been removed",
                    record.id
                );
            }
            // Persist a candidate before publishing it. Otherwise a failed Rename
            // can leak into live Status and an unrelated later activity write can
            // commit that rejected selector outside the registry lock.
            let mut candidate = record.clone();
            update(&mut candidate);
            candidate.updated_at_ms = now_ms();
            match atomic_write_json(&self.record_path, &candidate) {
                Ok(()) => {
                    *record = candidate.clone();
                    *lock(&self.record_persistence_error)? = None;
                }
                Err(error) => {
                    *lock(&self.record_persistence_error)? = Some(format!("{error:#}"));
                    return Err(error);
                }
            }
            candidate
        };
        // The subscriber push runs after the record guard is gone: every
        // broadcast takes the hub lock, and `mark_finalized` holds the hub
        // lock while taking the record one -- broadcasting inside the block
        // above would let record→hub meet hub→record and deadlock.
        self.output.broadcast_record(&published);
        Ok(published)
    }
    /// Refuse every later durable write (see `OutputHub::finalized`). Both
    /// locks are held while the flag is set so a writer that already holds
    /// either one finishes before the flag is observed, and any later
    /// writer observes it.
    pub(super) fn mark_finalized(&self) -> Result<()> {
        let _hub = lock(&self.output.inner)?;
        let _record = lock(&self.record)?;
        self.output.finalized.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub(super) fn send(&self, data: &[u8]) -> Result<()> {
        if !lock(&self.workload)?.running {
            bail!("workload has exited");
        }
        let file = lock(&self.pty_write)?
            .clone()
            .ok_or_else(|| anyhow!("PTY is closed"))?;
        // Unlocked: see `pty_write`.
        (&*file).write_all(data).context("write PTY")?;
        (&*file).flush()?;
        Ok(())
    }
    /// Apply a size while `terminal` is held, and repaint every attached
    /// client once it has actually landed. The shared-state check is what
    /// makes the common denominator cheap: a resize by a client that is not
    /// the smallest re-derives the size the PTY already has, returns here
    /// without a syscall, and -- the point of the policy -- without a
    /// SIGWINCH the workload would repaint at, or a repaint every client would
    /// have to absorb.
    pub(super) fn apply_size(
        &self,
        terminal: &mut TerminalState,
        rows: u16,
        cols: u16,
    ) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (terminal.rows, terminal.cols) == (rows, cols) {
            return Ok(());
        }
        let pty = lock(&self.pty_write)?;
        let file = pty.as_ref().ok_or_else(|| anyhow!("PTY is closed"))?;
        let previous_size = (terminal.rows, terminal.cols);
        resize_screen_and_pty(&self.output, previous_size, (rows, cols), || {
            set_winsize(file.as_raw_fd(), rows, cols)
        })?;
        terminal.rows = rows;
        terminal.cols = cols;
        // Only now, with the model reflowed and the PTY told: the snapshot
        // clients are about to receive is the screen at the new size. Still
        // the terminal -> hub order this path already uses; no hub call ever
        // takes `terminal`.
        self.output.broadcast_resize();
        Ok(())
    }

    /// Re-derive the shared size from the client registry and apply it.
    /// Every registry change goes through here -- attach, resize, detach --
    /// so there is exactly one definition of "the size every client can
    /// show", and one place it is published from. `terminal` is held.
    fn apply_common_size(&self, terminal: &mut TerminalState) -> Result<()> {
        match terminal.common_size() {
            Some((rows, cols)) => self.apply_size(terminal, rows, cols),
            // Nothing sized is attached any more: keep the PTY as it is, the
            // way tmux keeps a detached window's last size.
            None => Ok(()),
        }
    }

    /// Resizes the live screen model *before* the PTY ioctl (design doc
    /// section 5.3): output already in flight at the old size is parsed at
    /// the new one -- a transient tmux shares too -- but this ordering
    /// means a subsequent attach's snapshot is never rendered against a
    /// model that's still the wrong shape for the geometry the workload was
    /// just told about.
    ///
    /// The out-of-band RPC path, which has no client behind it: an explicit
    /// override, held until the next registry change re-derives the common
    /// denominator (there is no `window-size=manual`).
    pub(super) fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = screen::validate_worker_size(rows, cols)?;
        let mut terminal = lock(&self.terminal)?;
        self.apply_size(&mut terminal, rows, cols)
    }

    /// The geometry every attached client can show at once, or `None` while
    /// none of them has reported one. Read-only, and deliberately not a
    /// decision point: sizing goes through `apply_common_size`, so this is
    /// the same arithmetic a caller could do, and there is no second way to
    /// ask for a size.
    #[cfg(test)]
    pub(super) fn shared_size(&self) -> Option<(u16, u16)> {
        lock(&self.terminal)
            .ok()
            .and_then(|terminal| terminal.common_size())
    }

    /// Registers a subscriber and renders its initial payload while holding
    /// the client-size mutex. This makes geometry selection + snapshot one
    /// indivisible operation with respect to another client's attach, resize
    /// or detach, instead of allowing a concurrent client to change the
    /// model's dimensions between those two steps.
    pub(super) fn attach_client(
        &self,
        payload: AttachPayload,
        geometry: Option<(u16, u16)>,
        want_record: bool,
    ) -> Result<(u64, u64, Vec<u8>, OutputReceiver)> {
        let mut terminal = lock(&self.terminal)?;
        let client_id = terminal.next_client_id;
        terminal.next_client_id += 1;
        terminal
            .clients
            .insert(client_id, AttachedClient { geometry });
        // This client's geometry is now part of the common denominator, so a
        // phone attaching can shrink the shared screen under a desktop that
        // is already watching it. That is the point of the policy, and the
        // subscribers are told about it by `apply_size`. Best-effort: a
        // closing PTY must not refuse the attach.
        let _ = self.apply_common_size(&mut terminal);
        match self.output.subscribe(payload, want_record) {
            Ok((subscription, initial, rx)) => Ok((client_id, subscription, initial, rx)),
            Err(error) => {
                terminal.clients.remove(&client_id);
                // The attach was never established, so its geometry must not
                // keep holding the shared size down. No other client can race
                // us while `terminal` is held.
                let _ = self.apply_common_size(&mut terminal);
                Err(error)
            }
        }
    }

    /// Input reaches the PTY and nothing else. It deliberately does *not*
    /// re-derive the shared size: the common denominator does not depend on
    /// who is typing, and coupling the two is exactly what made a session
    /// watched from two devices resize back and forth on every keystroke.
    /// PTY writes can block behind a stopped or backpressured workload, so
    /// this holds no lock at all on the way in.
    pub(super) fn send_from_client(&self, _client_id: u64, data: &[u8]) -> Result<()> {
        self.send(data)
    }

    pub(super) fn resize_client(&self, client_id: u64, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = screen::validate_worker_size(rows, cols)?;
        let mut terminal = lock(&self.terminal)?;
        let client = terminal
            .clients
            .get_mut(&client_id)
            .ok_or_else(|| anyhow!("attached client is gone"))?;
        let previous_geometry = client.geometry;
        client.geometry = Some((rows, cols));
        if let Err(error) = self.apply_common_size(&mut terminal) {
            // A rejected size must not stick: it would keep clamping every
            // other client to a geometry this client never had.
            if let Some(client) = terminal.clients.get_mut(&client_id) {
                client.geometry = previous_geometry;
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn signal_from_client(&self, _client_id: u64, signal: i32) -> Result<()> {
        self.signal(signal)
    }

    /// The departing client's geometry leaves the common denominator, so the
    /// shared size grows to whatever is left -- a phone leaving hands the
    /// session back to the desktop's real size, in one step, with no
    /// keystroke needed. With no sized client left the PTY keeps the size it
    /// has, as tmux does for a window nothing is attached to.
    pub(super) fn detach_client(&self, client_id: u64) {
        let Ok(mut terminal) = self.terminal.lock() else {
            return;
        };
        terminal.clients.remove(&client_id);
        let _ = self.apply_common_size(&mut terminal);
    }
    pub(super) fn signal(&self, signal: i32) -> Result<()> {
        let workload = lock(&self.workload)?;
        if !workload.running {
            bail!("workload has exited");
        }
        if unsafe { libc::kill(-workload.pgid, signal) } != 0 {
            return Err(io::Error::last_os_error()).context("signal process group");
        }
        Ok(())
    }
    pub(super) fn kill(&self, signal: i32, grace_ms: u64) -> Result<()> {
        let grace = kill_grace_duration(grace_ms)?;
        let _serialized = lock(&self.kill_gate)?;
        if !self.workload_populated()? {
            // Already empty: no teardown will start from this call, and the
            // lifecycle thread owns the record from here (its ChildExit
            // handler writes `Exiting`, issue #18). Writing again here would
            // race that finalization's record removal and could resurrect a
            // removed state dir (atomic_write_json recreates parents).
            return Ok(());
        }
        // The kill is accepted and teardown is about to start: persist the
        // transition BEFORE signalling (issue #18). Until now the durable
        // record kept its pre-kill phase, so for the whole window between
        // the accepted Kill RPC and finalization's record removal -- bounded
        // by the CLI's KILL_RECORD_REMOVAL_WAIT, indefinite if finalization
        // wedges -- `a snapshot` reported the dying session byte-for-byte
        // like a healthy one. Writing under the same record mutex
        // `update_record` holds, and strictly before the signal that leads
        // to the ChildExit -> finalization sequence, this write is always
        // first: the removal that follows can only delete it, never be
        // preceded by it. Best-effort on purpose -- a failed write must not
        // stop a kill or flip the accepted RPC to an error; the next writer
        // (ChildExit handler, finalization) retries the transition or
        // removes the record outright, and `record_persistence_error` keeps
        // surfacing the failure meanwhile. The termination monitor's
        // SIGTERM teardown shares this method, which is the same dying
        // fact about that session and gets the same honest record.
        if let Err(error) = self.update_record(|record| record.phase = Phase::Exiting) {
            log_best_effort(&format!(
                "aplexer worker: mark accepted-kill session exiting: {error:#}"
            ));
        }
        let grace_deadline = Instant::now()
            .checked_add(grace)
            .ok_or_else(|| anyhow!("kill grace deadline overflow"))?;
        let cleanup_deadline = grace_deadline
            .checked_add(DESCENDANT_KILL_TIMEOUT)
            .ok_or_else(|| anyhow!("kill cleanup deadline overflow"))?;
        let cgroup = lock(&self.cgroup)?.clone();
        if signal == libc::SIGKILL {
            if let Some(cg) = &cgroup {
                cg.kill_all_until(cleanup_deadline)?;
            } else {
                kill_descendants(std::process::id(), DESCENDANT_KILL_TIMEOUT)?;
            }
            return Ok(());
        }
        if let Some(cg) = &cgroup {
            cg.signal_all_until(signal, cleanup_deadline)?;
        } else {
            signal_descendants(std::process::id(), signal)?;
        }
        // Poll instead of sleeping the whole grace period: once the workload
        // is gone there is nothing to escalate to SIGKILL, and the response
        // to this request should not be delayed (the worker exits shortly
        // after the workload does, so a response stuck behind a long sleep
        // could be lost entirely). Polled at KILL_POLL_INTERVAL (5 ms), not
        // the 25 ms lifecycle cadence, so a workload that dies on the first
        // signal does not pay a quantization delay (benchmark PLAN P0.2).
        while self.workload_still_populated()? && Instant::now() < grace_deadline {
            thread::sleep(KILL_POLL_INTERVAL);
        }
        if self.workload_populated()? {
            if let Some(cg) = &cgroup {
                cg.kill_all_until(cleanup_deadline)?;
            } else {
                kill_descendants(std::process::id(), DESCENDANT_KILL_TIMEOUT)?;
            }
        }
        Ok(())
    }

    /// `workload_populated` for a poll loop: a process group that still
    /// answers `kill(-pgid, 0)` is populated without walking `/proc` at all.
    /// The 5 ms kill poll used to do a full descendant walk on every tick
    /// for an unlimited session -- dozens of `/proc/*/task/*/children`
    /// reads per tick while a workload ran out its grace window. Only a
    /// "yes" is taken from the probe: an empty group still needs the walk,
    /// because a `setsid` descendant leaves the group without leaving the
    /// domain. A zombie member keeps the group signalable until its parent
    /// (this worker's reaper thread, or the waiter for the leader) reaps
    /// it, which is immediate, so the answer is at most one poll late.
    pub(super) fn workload_still_populated(&self) -> Result<bool> {
        let signalable = {
            let workload = lock(&self.workload)?;
            workload.running && unsafe { libc::kill(-workload.pgid, 0) } == 0
        };
        if signalable {
            return Ok(true);
        }
        self.workload_populated()
    }

    /// Whether any process remains inside this session's containment domain.
    /// A leader exiting is not sufficient: a `setsid` descendant may have
    /// escaped the leader's process group while still belonging to the
    /// session. Limited sessions use the kernel's cgroup membership; ordinary
    /// sessions use the worker's subreaper descendant tree.
    pub(super) fn workload_populated(&self) -> Result<bool> {
        if let Some(cgroup) = lock(&self.cgroup)?.as_ref() {
            return cgroup.populated();
        }
        Ok(!descendant_pids(std::process::id())?.is_empty())
    }
    /// Rename this session within its workspace (or into a new one).
    ///
    /// The `workspace+tag` claim check answers the same question
    /// `start_session`'s supersede check answers -- "does any record still
    /// own this pair?" -- and it must get the same answer, so it runs
    /// through the very same decision and retirement `a start` uses
    /// (`claim_holder_pair` / `retire_reclaimed_holder`, no third copy).
    /// A holder that no longer needs the pair is retired exactly as a
    /// reclaiming start retires it -- archive, delete, drop runtime state,
    /// report an unproven reclaim -- which keeps the invariant that one
    /// pair has one record; an interim fix took the name and left the
    /// corpse for `a prune`, but that made pairs transiently multiply-held
    /// and every holder scan had to guess, so this supersedes it (issue
    /// #13). A live holder keeps its claim, and the refusal names its
    /// derived state and a next step, in `a start`'s own words.
    ///
    /// A pre-PID `Starting` holder is the one "dead"-looking shape that the
    /// fence inside the claim still refuses (issue #9): a stub in the
    /// spawn-to-worker-lock gap is a healthy session coming up, and its
    /// worker lock is both the detector and the fence.
    ///
    /// Every dead conflict is retired in turn, not just the first: a
    /// registry written while the interim fix was live can hold a pair
    /// more than once, and the loop drains them under the registry lock
    /// this rename already holds.
    pub(super) fn rename(
        &self,
        workspace: std::path::PathBuf,
        tag: String,
    ) -> Result<SessionRecord> {
        validate_tag(&tag)?;
        let workspace = canonical_workspace(&workspace)?;
        let _registry = registry_lock_within(&self.paths, RENAME_REGISTRY_WAIT)?;
        self.take_pair(&workspace, &tag)?;
        self.update_record(|r| {
            r.workspace = workspace;
            r.tag = tag;
        })
    }
    /// Retire every dead holder of the pair this rename wants, refusing --
    /// with the holder named and a way out -- if any of them still owns it.
    /// Registry contents cannot change under the caller's lock: a start
    /// writes its pre-PID stub holding the same lock, so once no conflict
    /// is listed, none can appear before the update above commits.
    fn take_pair(&self, workspace: &std::path::Path, tag: &str) -> Result<()> {
        while let Some(holder) = list_records(&self.paths)?.into_iter().find(|record| {
            record.id != self.id && record.workspace == workspace && record.tag == tag
        }) {
            let claim = claim_holder_pair(&self.paths, &holder)?;
            retire_reclaimed_holder(&self.paths, &holder, claim.verdict)?;
        }
        Ok(())
    }
    /// `a state-report <state>` (docs/pocketshell-integration-plan.md Open
    /// question #2): a hook running inside this session pushes its own
    /// semantic state. Validated here (not just at the CLI's `ValueEnum`
    /// layer) so a direct/malformed RPC from any caller can't write an
    /// unrecognised value into the record that `watch.rs`'s merge logic
    /// would then have to guess at -- the same defensive posture `rename`
    /// takes with `validate_tag` above.
    pub(super) fn report_state(&self, state: String) -> Result<SessionRecord> {
        validate_reported_state(&state)?;
        self.update_record(move |r| {
            r.reported_state = Some(state);
            r.reported_state_at_ms = Some(now_ms());
        })
    }
    /// `a agent <token>` / `a agent --clear` (`Operation::SetAgent`): pin
    /// which agent this session reports, or `None` to unpin and return to
    /// live detection. Detection reports the first agent the workload's
    /// process tree happens to hold, so after switching agents inside a
    /// session -- the new one launched from inside the old, or the old one
    /// merely suspended -- every surface kept naming the stale one, and
    /// nothing could correct it. The pin is the correction.
    ///
    /// The token is validated here (not just at the CLI layer), the same
    /// defensive posture `report_state` takes: a direct/malformed RPC from
    /// any caller must not write a pin every surface would silently ignore.
    /// Classification goes through the same config-derived table detection
    /// itself uses (`agent_kind::profile_variants`), so a pin spelled the
    /// way detection would spell it (`zcodex` -> codex/zcodex) resolves on
    /// every surface identically.
    pub(super) fn set_agent(&self, agent: Option<String>) -> Result<SessionRecord> {
        if let Some(token) = &agent {
            let config = crate::config::Config::load(&self.paths).ok();
            crate::agent_kind::validate_agent_token(token, config.as_ref())?;
        }
        self.update_record(move |r| {
            r.agent_override = agent;
        })
    }
}

/// Take the registry lock without blocking past `wait` (see
/// `RENAME_REGISTRY_WAIT`). A lock still held at the deadline is reported
/// as a distinct, retryable "registry is busy" error rather than as a
/// failed rename.
pub(super) fn registry_lock_within(paths: &Paths, wait: Duration) -> Result<FileLock> {
    let deadline = Instant::now() + wait;
    loop {
        match FileLock::exclusive(&paths.registry_lock(), true) {
            Ok(lock) => return Ok(lock),
            Err(error) if io_kind(&error) == Some(io::ErrorKind::WouldBlock) => {
                if Instant::now() >= deadline {
                    bail!(
                        "registry is busy (another aplexer command holds {}); retry the rename",
                        paths.registry_lock().display()
                    );
                }
                thread::sleep(DESCENDANT_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Keep the rendered model and kernel PTY geometry transactional. The model
/// must be resized first so concurrent output is parsed at the intended new
/// dimensions, but a rejected ioctl must not leave future snapshots claiming
/// a size the workload never received.
pub(super) fn resize_screen_and_pty(
    output: &OutputHub,
    previous_size: (u16, u16),
    new_size: (u16, u16),
    resize_pty: impl FnOnce() -> Result<()>,
) -> Result<()> {
    output.set_size(new_size.0, new_size.1)?;
    if let Err(error) = resize_pty() {
        if let Err(rollback_error) = output.set_size(previous_size.0, previous_size.1) {
            log_best_effort(&format!(
                "aplexer worker: roll back screen after PTY resize failure: {rollback_error:#}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

/// The worker's lock-poisoning policy for shared state: fail the operation.
///
/// A poisoned mutex means a thread panicked while the state was
/// mid-update, so the record, the PTY handle, the client registry, the
/// hub's history-and-screen model, or the kill gate may be inconsistent;
/// every caller propagates the error (an RPC answers with it, a background
/// thread logs it) rather than acting on state it cannot trust. The one
/// deliberate exception is the per-subscriber queue in `hub.rs`
/// (`SubscriberShared::poisoned_lock`): that state belongs to exactly one
/// attached client, its worst inconsistency is a dropped chunk for that
/// client, and a panic on one client's writer thread must not take every
/// other client's queue -- or the PTY reader that fans out to them -- down
/// with it.
pub(super) fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| anyhow!("worker lock poisoned"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::hub::tests::test_hub;

    #[test]
    pub(super) fn failed_pty_resize_restores_the_previous_screen_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let before = hub.screen_snapshot().unwrap();

        let error = resize_screen_and_pty(&hub, (24, 80), (10, 20), || {
            bail!("injected PTY ioctl failure")
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "injected PTY ioctl failure");
        assert_eq!(
            hub.screen_snapshot().unwrap(),
            before,
            "failed PTY resize left the screen model at the rejected size"
        );
    }

    #[test]
    pub(super) fn one_row_resize_normalizes_worker_pty_and_survives_round_trips() {
        fn pty_size(fd: std::os::fd::RawFd) -> (u16, u16) {
            let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
            assert_eq!(
                unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) },
                0,
                "read worker PTY winsize"
            );
            let size = unsafe { size.assume_init() };
            (size.ws_row, size.ws_col)
        }

        let dir = tempfile::tempdir().unwrap();
        let (master, _slave) = crate::process::open_pty(24, 80).unwrap();
        let observed_master = master.try_clone().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("session.json"));
        *runtime.pty_write.lock().unwrap() = Some(Arc::new(master));
        let (client_id, _, _, rx) = runtime
            .attach_client(AttachPayload::Tail(None), None, false)
            .unwrap();

        // The outer SSH PTY may accept 37x1. The aplexer-owned workload PTY
        // and vt100 model share the safe effective geometry 37x2.
        runtime.resize_client(client_id, 1, 37).unwrap();
        assert_eq!(pty_size(observed_master.as_raw_fd()), (2, 37));
        {
            let terminal = runtime.terminal.lock().unwrap();
            assert_eq!((terminal.rows, terminal.cols), (2, 37));
        }
        assert_eq!(
            runtime.output.inner.lock().unwrap().screen.rows(),
            2,
            "the screen model must use the same normalized height as the worker PTY"
        );

        let first = b"\r\nPS2884_RESUMED_READY_ps2856repro09240145\r\n";
        runtime.output.append(first).unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Data(data) if &data[..] == first
        ));

        runtime.resize_client(client_id, 17, 37).unwrap();
        assert_eq!(pty_size(observed_master.as_raw_fd()), (17, 37));
        assert_eq!(runtime.output.inner.lock().unwrap().screen.rows(), 17);

        let second = b"\r\nPS2884_OUTPUT_AFTER_GROWTH\r\n";
        runtime.output.append(second).unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Data(data) if &data[..] == second
        ));

        // Repeated shrinking and growing must continue to synchronize the
        // parser and the actual kernel PTY while preserving raw attach bytes.
        runtime.resize_client(client_id, 1, 37).unwrap();
        assert_eq!(pty_size(observed_master.as_raw_fd()), (2, 37));
        runtime.resize_client(client_id, 23, 37).unwrap();
        assert_eq!(pty_size(observed_master.as_raw_fd()), (23, 37));
        assert_eq!(runtime.output.inner.lock().unwrap().screen.rows(), 23);

        assert_eq!(
            runtime.output.snapshot(None).unwrap(),
            [first.as_slice(), second.as_slice()].concat(),
            "resize normalization must not change or drop bytes delivered to raw attach"
        );
        let screen = runtime.output.screen_contents().unwrap();
        assert!(screen.contains("PS2884_OUTPUT_AFTER_GROWTH"));
    }

    fn pty_size(fd: std::os::fd::RawFd) -> (u16, u16) {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        assert_eq!(
            unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) },
            0,
            "read worker PTY winsize"
        );
        let size = unsafe { size.assume_init() };
        (size.ws_row, size.ws_col)
    }

    /// A runtime whose PTY is a real pty pair, so the size the workload would
    /// see is observable rather than inferred.
    fn test_runtime_with_pty(dir: &tempfile::TempDir) -> (WorkerRuntime, Arc<File>) {
        let (master, _slave) = crate::process::open_pty(24, 80).unwrap();
        let observed = master.try_clone().unwrap();
        let runtime = test_runtime(dir, dir.path().join("session.json"));
        *runtime.pty_write.lock().unwrap() = Some(Arc::new(master));
        (runtime, Arc::new(observed))
    }

    /// The whole point of the policy, as a unit test: with two clients
    /// attached, the shared PTY is the smallest of their geometries, *input
    /// from either one cannot move it*, a resize by the client that is not
    /// the smallest cannot move it either, and the smallest one leaving hands
    /// the size straight back to what is left.
    ///
    /// The old `window-size=latest` behavior fails the second and third
    /// assertions: every keystroke flipped the PTY to the typing client's own
    /// geometry, which is what made a session watched from a laptop and a
    /// phone resize back and forth forever.
    #[test]
    pub(super) fn the_shared_pty_is_the_smallest_attached_geometry_and_input_cannot_move_it() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, pty) = test_runtime_with_pty(&dir);
        let (desktop, _, _, _) = runtime
            .attach_client(AttachPayload::Tail(None), Some((30, 100)), false)
            .unwrap();
        let (phone, _, _, _) = runtime
            .attach_client(AttachPayload::Tail(None), Some((20, 70)), false)
            .unwrap();
        assert_eq!(
            pty_size(pty.as_raw_fd()),
            (20, 70),
            "the phone attached last but the shared size is the common denominator, not the latest"
        );
        assert_eq!(runtime.shared_size(), Some((20, 70)));

        // Input, in both directions, must leave the geometry alone.
        runtime.send_from_client(desktop, b"x").unwrap();
        runtime.send_from_client(phone, b"y").unwrap();
        assert_eq!(
            pty_size(pty.as_raw_fd()),
            (20, 70),
            "typing on either client resized the shared PTY"
        );

        // Growing the client that is not the smallest changes nothing: it
        // still fits, so the shared screen is still the phone's.
        runtime.resize_client(desktop, 40, 120).unwrap();
        assert_eq!(
            pty_size(pty.as_raw_fd()),
            (20, 70),
            "a resize by a non-smallest client moved the shared PTY"
        );
        // Shrinking it below the phone's size does move it, in one step.
        runtime.resize_client(desktop, 12, 50).unwrap();
        assert_eq!(pty_size(pty.as_raw_fd()), (12, 50));
        // And the componentwise minimum is not "the smallest client": the
        // desktop is smaller in rows, the phone in columns, so the shared
        // screen is the pair of minima and neither client's own shape.
        runtime.resize_client(desktop, 10, 200).unwrap();
        runtime.resize_client(phone, 50, 40).unwrap();
        assert_eq!(
            pty_size(pty.as_raw_fd()),
            (10, 40),
            "the common denominator must be the componentwise minimum, not one client's geometry"
        );

        // The client that was holding the size down leaves: the remainder's
        // own geometry, with no keystroke and no resize from anyone.
        runtime.detach_client(desktop);
        assert_eq!(
            pty_size(pty.as_raw_fd()),
            (50, 40),
            "the smallest client leaving did not hand the size to what is left"
        );
        runtime.detach_client(phone);
        assert_eq!(
            pty_size(pty.as_raw_fd()),
            (50, 40),
            "the last client leaving must leave the PTY as it is, as tmux does"
        );
    }

    /// A geometry the worker refuses must not enter the registry: kept, it
    /// would clamp every other client to a size this client never had, for as
    /// long as it stayed attached.
    #[test]
    pub(super) fn a_rejected_geometry_never_reaches_the_common_denominator() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, pty) = test_runtime_with_pty(&dir);
        runtime
            .attach_client(AttachPayload::Tail(None), Some((30, 100)), false)
            .unwrap();
        let (other, _, _, _) = runtime
            .attach_client(AttachPayload::Tail(None), Some((20, 70)), false)
            .unwrap();

        let error = runtime
            .resize_client(other, u16::MAX, u16::MAX)
            .expect_err("an oversized geometry must be refused");
        assert!(format!("{error:#}").contains("cells"), "{error:#}");
        assert_eq!(
            pty_size(pty.as_raw_fd()),
            (20, 70),
            "a refused geometry still moved the shared PTY"
        );

        // The refused client's remembered geometry is the one it last had, so
        // it keeps clamping only for as long as it says so.
        assert_eq!(
            runtime
                .terminal
                .lock()
                .unwrap()
                .clients
                .get(&other)
                .expect("the refused client is still attached")
                .geometry,
            Some((20, 70)),
            "the refused geometry replaced the client's last valid one"
        );
        runtime.detach_client(other);
        assert_eq!(pty_size(pty.as_raw_fd()), (30, 100));
        assert_eq!(runtime.shared_size(), Some((30, 100)));
    }

    /// A client that never reported a geometry (a raw-tail `--history-bytes`
    /// attach) is a viewer, not a constraint: it must be able to watch a
    /// session at whatever size the sized clients agree on.
    #[test]
    pub(super) fn a_geometry_less_client_does_not_constrain_the_shared_size() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, pty) = test_runtime_with_pty(&dir);
        let (sized, _, _, _) = runtime
            .attach_client(AttachPayload::Tail(None), Some((24, 80)), false)
            .unwrap();
        let (viewer, _, _, _) = runtime
            .attach_client(AttachPayload::Tail(None), None, false)
            .unwrap();
        assert_eq!(pty_size(pty.as_raw_fd()), (24, 80));
        assert_eq!(runtime.shared_size(), Some((24, 80)));
        assert_eq!(
            runtime
                .terminal
                .lock()
                .unwrap()
                .clients
                .get(&viewer)
                .expect("the viewer is still attached")
                .geometry,
            None
        );
        assert!(runtime
            .terminal
            .lock()
            .unwrap()
            .clients
            .contains_key(&sized));
    }

    /// Every move of the shared size repaints the clients that render the
    /// screen, and nothing else does: a client whose terminal is not the
    /// binding constraint must not be made to absorb a repaint for a resize
    /// that did not happen, and a raw-tail subscriber's byte-exact stream
    /// must never gain a repaint in the middle of it.
    ///
    /// The repaint is the *snapshot*, not a size announcement, because the
    /// worker's grid is the authoritative one: it reflowed the content on the
    /// shrink and it is the only party that knows what the result looks like.
    /// A client whose terminal is larger than the shared screen also gets its
    /// stale rows and columns cleared, since the snapshot's Erase in Display
    /// covers them in the client's own model too.
    #[test]
    pub(super) fn only_real_size_changes_repaint_screen_subscribers() {
        let dir = tempfile::tempdir().unwrap();
        // A real PTY: `apply_size` publishes only after the ioctl lands, and
        // the fixture's `/dev/null` handle has no winsize to set.
        let (runtime, _pty) = test_runtime_with_pty(&dir);
        // The screen starts 24x80, so put a marker on its last row: the
        // repaint that follows has to be a rendering of the *current* grid,
        // not a replay of what the workload wrote.
        runtime.output.append(b"\x1b[24;1HLAST-ROW-BEFORE").unwrap();
        let (_, _, screen_rx) = runtime
            .output
            .subscribe(AttachPayload::Screen, false)
            .unwrap();
        let (_, _, tail_rx) = runtime
            .output
            .subscribe(AttachPayload::Tail(None), false)
            .unwrap();

        let expect_repaint = |rx: &OutputReceiver, rows: u16, cols: u16, what: &str| {
            let OutputEvent::Data(data) = rx.recv().unwrap() else {
                panic!("{what}: expected the repaint snapshot");
            };
            let mut screen = crate::screen::ScreenTracker::try_new(rows, cols).unwrap();
            screen.process(&data);
            assert!(
                screen.contents().contains("LAST-ROW-BEFORE"),
                "{what}: the repaint is not the screen at the new size"
            );
            // ...and the layout nudge, because the snapshot's own ED2 ignores
            // scroll margins and so can wipe the client's reserved row.
            match rx.recv().unwrap() {
                OutputEvent::Layout(screen::LayoutChange {
                    erase_reset: true, ..
                }) => {}
                other => panic!("{what}: expected the layout nudge, got {other:?}"),
            }
        };

        // The subscriber is already attached, so it sees a repaint per real
        // move: the desktop's own attach, then the phone dragging the screen
        // down under it.
        let (desktop, _, _, _) = runtime
            .attach_client(AttachPayload::Tail(None), Some((30, 100)), false)
            .unwrap();
        expect_repaint(&screen_rx, 30, 100, "the desktop's attach");
        let (phone, _, _, _) = runtime
            .attach_client(AttachPayload::Tail(None), Some((20, 70)), false)
            .unwrap();
        expect_repaint(&screen_rx, 20, 70, "the phone's attach");
        assert!(
            tail_rx.try_recv().is_none(),
            "a raw-tail subscriber was handed a repaint inside its byte-exact stream"
        );

        // Growing the desktop: still above the phone, so nothing moves and
        // nothing is repainted.
        runtime.resize_client(desktop, 40, 120).unwrap();
        assert_eq!(runtime.shared_size(), Some((20, 70)));
        assert!(
            screen_rx.try_recv().is_none(),
            "a resize that did not move the shared size repainted anyway"
        );
        assert!(tail_rx.try_recv().is_none());

        // The phone leaving does move it, and every screen subscriber is
        // repainted at the size that is left.
        runtime.detach_client(phone);
        expect_repaint(&screen_rx, 40, 120, "the phone's detach");
        assert!(tail_rx.try_recv().is_none());
    }

    /// A `WorkerRuntime` over a throwaway hub, with its durable record at
    /// `record_path` and every other path under `dir`.
    pub(super) fn test_runtime(
        dir: &tempfile::TempDir,
        record_path: std::path::PathBuf,
    ) -> WorkerRuntime {
        let mut record = SessionRecord::fixture(dir.path(), "before");
        record.socket_path = dir.path().join("control.sock");
        record.history_path = dir.path().join("history.bin");
        WorkerRuntime {
            id: record.id,
            paths: Paths {
                runtime_root: dir.path().join("runtime"),
                state_root: dir.path().join("state"),
                config_file: dir.path().join("config.toml"),
            },
            record_path,
            runtime_session_dir: dir.path().join("runtime-session"),
            socket_path: dir.path().join("control.sock"),
            record: Mutex::new(record),
            pty_write: Mutex::new(Some(Arc::new(File::open("/dev/null").unwrap()))),
            workload: Mutex::new(WorkloadState {
                running: true,
                pgid: 1,
            }),
            terminal: Mutex::new(TerminalState {
                rows: 24,
                cols: 80,
                clients: HashMap::new(),
                next_client_id: 1,
            }),
            cgroup: Mutex::new(None),
            kill_gate: Mutex::new(()),
            output: test_hub(dir),
            record_persistence_error: Mutex::new(None),
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    #[test]
    pub(super) fn failed_record_persistence_does_not_publish_and_idle_activity_retries() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("session.json");
        // Atomic rename onto a directory deterministically fails after the
        // candidate was serialized, exercising the publish boundary.
        fs::create_dir(&record_path).unwrap();
        let runtime = test_runtime(&dir, record_path);

        assert!(runtime
            .update_record(|candidate| candidate.tag = "after".into())
            .is_err());
        assert_eq!(runtime.record().unwrap().tag, "before");
        assert!(runtime.record_persistence_error.lock().unwrap().is_some());

        runtime.last_activity_ms.store(123, Ordering::Relaxed);
        let mut persisted_activity_ms = 0;
        assert!(persist_activity_checkpoint(&runtime, &mut persisted_activity_ms).is_err());
        assert_eq!(persisted_activity_ms, 0, "failed write advanced checkpoint");
        assert_eq!(runtime.record().unwrap().last_activity_ms, None);

        // No new activity occurs between attempts. Once the transient
        // destination failure is removed, the unchanged timestamp must still
        // be retried and published by the next tick.
        fs::remove_dir(&runtime.record_path).unwrap();
        persist_activity_checkpoint(&runtime, &mut persisted_activity_ms).unwrap();
        assert_eq!(persisted_activity_ms, 123);
        assert_eq!(runtime.record().unwrap().last_activity_ms, Some(123));
        assert!(runtime.record_persistence_error.lock().unwrap().is_none());
    }

    /// The flush-loop tick must survive persistence failures: return, log
    /// best-effort, leave the checkpoint unadvanced so the next tick retries.
    /// The loop around it died once for good when its `eprintln!` reporting a
    /// full disk panicked because worker.log sat on the same full disk --
    /// with the thread gone, neither history nor `last_activity_ms` ever
    /// reached disk again, and the attach bar read IDLE through live turns.
    #[test]
    pub(super) fn flush_tick_survives_and_defers_failed_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("session.json");
        // Atomic rename onto a directory deterministically fails after the
        // candidate was serialized (same trick as the test above).
        fs::create_dir(&record_path).unwrap();
        let runtime = test_runtime(&dir, record_path);
        runtime.output.append(b"history").unwrap();
        runtime.last_activity_ms.store(7, Ordering::Relaxed);

        let mut persisted_activity_ms = 0u64;
        flush_tick(&runtime, &mut persisted_activity_ms);

        assert_eq!(persisted_activity_ms, 0, "failed checkpoint advanced");
        assert!(runtime.record_persistence_error.lock().unwrap().is_some());
    }

    /// The seam behind `log_best_effort` never unwinds on a failed write:
    /// a dropped diagnostic line is fine, a dead worker thread is not.
    #[test]
    pub(super) fn log_line_to_a_failing_writer_is_dropped_not_panicked() {
        struct AlwaysFull;
        impl std::io::Write for AlwaysFull {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = AlwaysFull;
        assert!(write_log_line(&mut writer, "flush history: No space left").is_err());
    }

    /// The Status payload serves the PTY reader's live activity atomic over
    /// the record's field whenever the atomic is ahead. The record's field
    /// only advances when a disk write lands, so during a persistence
    /// failure window the served value would otherwise freeze -- and with it
    /// the recency heuristic and the `idle`-push retraction rule that the
    /// attach bar's state derivation consumes.
    #[test]
    pub(super) fn status_serves_the_live_activity_atomic_over_a_frozen_record() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("session.json"));
        runtime
            .update_record(|record| record.last_activity_ms = Some(1_000))
            .unwrap();
        runtime.last_activity_ms.store(2_000, Ordering::Relaxed);

        let served = connection::status_value(&runtime).unwrap();
        assert_eq!(served["last_activity_ms"].as_u64(), Some(2_000));

        // A record the atomic has not caught up with (no output seen yet,
        // or the reader behind the last durable write) is left standing.
        runtime.last_activity_ms.store(0, Ordering::Relaxed);
        let served = connection::status_value(&runtime).unwrap();
        assert_eq!(served["last_activity_ms"].as_u64(), Some(1_000));
    }

    /// The one funnel: a committed record write pushes the fresh record to
    /// subscribers that opted in (`want_record`), which is how a rename
    /// issued by a second client reaches an attached status bar within one
    /// round-trip instead of at that client's next poll.
    #[test]
    pub(super) fn committed_record_write_pushes_the_fresh_record_to_subscribers() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("session.json"));
        let (_, _, rx) = runtime
            .output
            .subscribe(AttachPayload::Screen, true)
            .unwrap();

        let renamed = runtime
            .update_record(|record| record.tag = "renamed".into())
            .unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::RecordUpdated(got)
                if got.id == renamed.id && got.tag == "renamed"
        ));
    }

    /// A write that never committed must push nothing: the rejected
    /// candidate must not leak onto the wire (the same
    /// persist-before-publish rule `update_record` applies to the in-memory
    /// record).
    #[test]
    pub(super) fn failed_record_persistence_pushes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("session.json");
        // Atomic rename onto a directory deterministically fails after the
        // candidate was serialized (same trick as the test above).
        fs::create_dir(&record_path).unwrap();
        let runtime = test_runtime(&dir, record_path);
        let (_, _, rx) = runtime
            .output
            .subscribe(AttachPayload::Screen, true)
            .unwrap();

        assert!(runtime
            .update_record(|record| record.tag = "after".into())
            .is_err());
        runtime.output.fail_subscribers("done".into());
        assert!(
            matches!(rx.recv().unwrap(), OutputEvent::Error(_)),
            "no RecordUpdated may precede the terminal event"
        );
    }

    /// The clean-exit resurrection: `run_lifecycle` removes the state dir,
    /// then drains connections for up to 3 s before exiting, and in that
    /// window the periodic flush thread and a late attach both wrote into
    /// the removed directory (`atomic_write_*` recreates parents), leaving
    /// a `phase: exiting` record with a dead worker pid for `a list` to
    /// show as broken until `a prune`. Pins that once the lifecycle marks
    /// the session finalized, neither the record writer nor the history
    /// flusher recreates anything -- whether the write is an activity
    /// checkpoint, an attach stamp, or a forced flush.
    #[test]
    pub(super) fn finalized_session_refuses_every_later_durable_write() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state-session");
        let runtime = test_runtime(&dir, state_dir.join("session.json"));
        // Sanity: before finalization the same writers land on disk.
        runtime
            .update_record(|record| record.tag = "live".into())
            .expect("record write before finalization");
        runtime.output.append(b"output").unwrap();
        runtime.output.flush_history(true).unwrap();
        assert!(runtime.record_path.exists());
        assert!(dir.path().join("history.bin").exists());

        runtime.mark_finalized().unwrap();
        fs::remove_dir_all(&state_dir).unwrap();
        fs::remove_file(dir.path().join("history.bin")).unwrap();

        let error = runtime
            .update_record(|record| record.last_accessed_ms = Some(now_ms()))
            .expect_err("a finalized session must refuse record writes");
        assert!(format!("{error:#}").contains("finalized"), "{error:#}");
        assert!(
            runtime.record_persistence_error.lock().unwrap().is_none(),
            "a refused post-finalization write is not a persistence failure"
        );
        runtime.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        let mut persisted_activity_ms = 0;
        persist_activity_checkpoint(&runtime, &mut persisted_activity_ms)
            .expect("the activity checkpoint quietly skips a finalized session");
        runtime.output.append(b"late output").unwrap();
        runtime.output.flush_history(false).unwrap();
        runtime.output.flush_history(true).unwrap();

        assert!(
            !state_dir.exists(),
            "a durable write after finalization resurrected the state dir"
        );
        assert!(
            !dir.path().join("history.bin").exists(),
            "a history flush after finalization resurrected the history file"
        );
    }

    /// `a start` holds the registry lock for its whole spawn-and-poll; a
    /// rename that blocked on it outlived the client's control deadline
    /// and then applied unobserved. It must refuse instead, in time, with
    /// an error that says the registry is busy and nothing changed.
    #[test]
    pub(super) fn rename_refuses_in_time_while_the_registry_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("session.json"));
        runtime.paths.ensure().unwrap();
        let _held = FileLock::exclusive(&runtime.paths.registry_lock(), true).unwrap();

        let started = Instant::now();
        let error = runtime
            .rename(dir.path().to_path_buf(), "renamed".into())
            .expect_err("rename must not wait out a held registry lock");
        let elapsed = started.elapsed();
        assert!(
            format!("{error:#}").contains("registry is busy"),
            "{error:#}"
        );
        assert!(
            elapsed >= RENAME_REGISTRY_WAIT
                && elapsed < RENAME_REGISTRY_WAIT + Duration::from_secs(1),
            "rename gave up after {elapsed:?}, expected about {RENAME_REGISTRY_WAIT:?}"
        );
        assert_eq!(runtime.record().unwrap().tag, "before");
        assert!(
            !runtime.record_path.exists(),
            "a refused rename wrote the record"
        );
    }
}
