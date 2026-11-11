//! Fail-closed submission. A durable uncertain fence precedes every input byte.
use super::runtime::StrictDeadline;
use super::*;

#[cfg(test)]
thread_local! {
    // Per-test-thread delays model slow durable IO/readiness without any
    // production fault-injection switch or cross-session interference.
    static STAGE_DELAY: std::cell::Cell<Option<(&'static str, Duration)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn delay_stage(stage: &str) {
    STAGE_DELAY.with(|delay| {
        if let Some((name, duration)) = delay.get() {
            if name == stage {
                delay.set(None);
                thread::sleep(duration);
            }
        }
    });
}

impl WorkerRuntime {
    #[cfg_attr(not(target_os = "linux"), allow(unreachable_code))]
    pub(super) fn submit_strict(
        &self,
        operation_id: Uuid,
        expected_tag: &str,
        expected_workspace: &Path,
        expected_created_at_ms: u64,
        deadline_ms: u64,
        text: &str,
    ) -> Result<serde_json::Value> {
        let digest = crate::strict_submission_digest(
            self.id,
            operation_id,
            expected_tag,
            expected_workspace,
            expected_created_at_ms,
            deadline_ms,
            text,
        )?;
        let base = json!({"version":1,"session_id":self.id,"operation_id":operation_id,
            "request_sha256":digest,"deadline_ms":deadline_ms,"status":"refused","mutation":false,
            "reason":"preflight_refused","landed":false,"submitted":false,"consumed":null});
        let outcome = |status: &str, reason: &str, mutation: serde_json::Value| {
            let mut value = base.clone();
            value["status"] = json!(status);
            value["reason"] = json!(reason);
            value["mutation"] = mutation;
            value
        };
        // Other platforms remain explicitly unavailable until equivalent
        // secure regular-file/no-reparse receipt opening is proved.
        #[cfg(not(target_os = "linux"))]
        return Ok(outcome(
            "refused",
            "strict_platform_unsupported",
            json!(false),
        ));
        let _input = match self.input_gate.try_lock() {
            Ok(guard) => guard,
            // Another attempt may own this same operation. Never promise a
            // global no-dispatch result without observing its durable fence.
            Err(_) => return Ok(outcome("uncertain", "input_busy", serde_json::Value::Null)),
        };
        if text.trim().is_empty()
            || text.len() > 8192
            || text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
        {
            return Ok(outcome("refused", "invalid_text", json!(false)));
        }
        let receipt_path = self
            .paths
            .state_session(self.id)
            .join(format!("strict-send-{operation_id}.json"));
        match read_receipt(&receipt_path) {
            Ok(Some(prior)) => {
                if prior["version"] != 1
                    || prior["session_id"] != self.id.to_string()
                    || prior["operation_id"] != operation_id.to_string()
                    || prior["request_sha256"] != digest
                    || prior["deadline_ms"] != deadline_ms
                    || !matches!(
                        prior["status"].as_str(),
                        Some("uncertain" | "submitted_verified")
                    )
                {
                    return Ok(outcome(
                        "uncertain",
                        "operation_conflict",
                        serde_json::Value::Null,
                    ));
                }
                if prior["mutation"] != true
                    || !matches!(prior["landed"], serde_json::Value::Bool(_))
                    || !matches!(prior["submitted"], serde_json::Value::Bool(_))
                    || (prior["status"] == "submitted_verified"
                        && (prior["landed"] != true
                            || prior["submitted"] != true
                            || prior["reason"] != "submission_observed"))
                {
                    return Ok(outcome(
                        "uncertain",
                        "receipt_unreadable",
                        serde_json::Value::Null,
                    ));
                }
                let mut replay = base.clone();
                for key in ["status", "mutation", "landed", "submitted"] {
                    replay[key] = prior[key].clone();
                }
                replay["reason"] = json!(if prior["status"] == "submitted_verified" {
                    "submission_observed"
                } else {
                    "prior_dispatch_uncertain"
                });
                replay["replayed"] = json!(true);
                return Ok(replay);
            }
            Ok(None) => {}
            Err(_) => {
                return Ok(outcome(
                    "uncertain",
                    "receipt_unreadable",
                    serde_json::Value::Null,
                ))
            }
        }
        let Some(deadline) = StrictDeadline::new(deadline_ms) else {
            return Ok(outcome("refused", "deadline_expired", json!(false)));
        };
        let record = self.record()?;
        if record.id != self.id
            || record.tag != expected_tag
            || record.workspace != expected_workspace
            || record.created_at_ms != expected_created_at_ms
            || !expected_workspace.is_absolute()
        {
            return Ok(outcome("refused", "identity_mismatch", json!(false)));
        }
        if !lock(&self.workload)?.running
            || record.phase != Phase::Running
            || record.reported_state.as_deref() == Some("working")
        {
            return Ok(outcome("refused", "not_idle", json!(false)));
        }
        if !lock(&self.terminal)?.clients.is_empty() {
            return Ok(outcome("refused", "input_client_attached", json!(false)));
        }
        if deadline.expired() {
            return Ok(outcome("refused", "deadline_expired", json!(false)));
        }
        let (before, styled, generation) = self.output.strict_input_snapshot()?;
        if !self.input_is_settled_at(generation)? {
            return Ok(outcome("refused", "input_not_settled", json!(false)));
        }
        let Some(family) = crate::delivery_ack::resolve_family(&record.engine, &before) else {
            return Ok(outcome("refused", "unknown_layout", json!(false)));
        };
        if crate::delivery_ack::parse_screen(family, &before).is_none() {
            return Ok(outcome("refused", "unknown_layout", json!(false)));
        }
        if !crate::delivery_ack::screen_from_snapshot(&styled)
            .screen()
            .bracketed_paste()
        {
            return Ok(outcome(
                "refused",
                "bracketed_paste_unavailable",
                json!(false),
            ));
        }
        if !crate::delivery_ack::screen_is_ready_idle(
            family,
            &before,
            crate::delivery_ack::screen_from_snapshot(&styled).screen(),
        ) {
            return Ok(outcome("refused", "unsafe_composer", json!(false)));
        }
        // Reuse the already-established native record directory. A first-use
        // journal subdirectory would need its own parent-entry durability fence.
        if !receipt_path.parent().is_some_and(|parent| parent.is_dir()) {
            return Ok(outcome(
                "refused",
                "session_state_unavailable",
                json!(false),
            ));
        }
        if deadline.expired() {
            return Ok(outcome("refused", "deadline_expired", json!(false)));
        }
        let mut receipt = outcome("uncertain", "dispatch_fenced", json!(true));
        let persisted = (|| -> Result<()> {
            crate::atomic_write_json(&receipt_path, &receipt)?;
            #[cfg(test)]
            delay_stage("fsync");
            // atomic_write_json syncs the file and its record directory. Also
            // fence that existing directory's entry and the sessions entry,
            // without relying on a first-use mkdir becoming durable by luck.
            crate::persist::sync_dir(&self.paths.state_root.join("sessions"))?;
            crate::persist::sync_dir(&self.paths.state_root)?;
            Ok(())
        })();
        if persisted.is_err() {
            return Ok(outcome(
                "uncertain",
                "receipt_persistence_failed",
                serde_json::Value::Null,
            ));
        }
        // Persistence can take time: recheck current readiness before mutation.
        if deadline.expired() {
            receipt["reason"] = json!("deadline_expired");
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        let (plain, styled, generation) = self.output.strict_input_snapshot()?;
        if !self.input_is_settled_at(generation)?
            || !crate::delivery_ack::screen_from_snapshot(&styled)
                .screen()
                .bracketed_paste()
            || !crate::delivery_ack::screen_is_ready_idle(
                family,
                &plain,
                crate::delivery_ack::screen_from_snapshot(&styled).screen(),
            )
        {
            // The committed fence stays uncertain, not eligible for replay.
            receipt["reason"] = json!("readiness_changed");
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        // One framed paste, at most one Enter, no resend or fallback. No
        // record/screen mutex is held across potentially blocking PTY writes.
        #[cfg(test)]
        delay_stage("paste");
        if deadline.expired() {
            receipt["reason"] = json!("deadline_expired");
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        let paste = format!("\x1b[200~{text}\x1b[201~");
        if self
            .send_under_input_lease_before(paste.as_bytes(), Some(deadline))
            .is_err()
        {
            receipt["reason"] = json!(if deadline.expired() {
                "deadline_expired"
            } else {
                "paste_uncertain"
            });
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        let landing_until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < landing_until && !deadline.expired() {
            let (plain, _, _) = self.output.strict_input_snapshot()?;
            if deadline.expired() {
                break;
            }
            if crate::delivery_ack::strict_payload_landed(family, &plain, text) {
                receipt["landed"] = json!(true);
                break;
            }
            thread::sleep(Duration::from_millis(25).min(deadline.remaining()));
        }
        if receipt["landed"] != true {
            receipt["reason"] = json!(if deadline.expired() {
                "deadline_expired"
            } else {
                "landing_uncertain"
            });
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        if deadline.expired() {
            receipt["reason"] = json!("deadline_expired");
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        let (plain, _, _) = self.output.strict_input_snapshot()?;
        if !crate::delivery_ack::strict_payload_landed(family, &plain, text) {
            receipt["reason"] = json!("landing_changed");
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        #[cfg(test)]
        delay_stage("enter");
        if deadline.expired() {
            receipt["reason"] = json!("deadline_expired");
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        if self
            .send_under_input_lease_before(b"\r", Some(deadline))
            .is_err()
        {
            receipt["reason"] = json!(if deadline.expired() {
                "deadline_expired"
            } else {
                "enter_uncertain"
            });
            let _ = crate::atomic_write_json(&receipt_path, &receipt);
            return Ok(receipt);
        }
        let submission_until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < submission_until && !deadline.expired() {
            let (plain, styled, _generation) = self.output.strict_input_snapshot()?;
            if deadline.expired() {
                break;
            }
            if crate::delivery_ack::strict_submission_confirmed(
                family,
                &before,
                &plain,
                text,
                crate::delivery_ack::screen_from_snapshot(&styled).screen(),
            ) {
                receipt["status"] = json!("submitted_verified");
                receipt["submitted"] = json!(true);
                receipt["reason"] = json!("submission_observed");
                if crate::atomic_write_json(&receipt_path, &receipt).is_err() {
                    return Ok(outcome(
                        "uncertain",
                        "receipt_persistence_failed",
                        json!(true),
                    ));
                }
                return Ok(receipt);
            }
            thread::sleep(Duration::from_millis(25).min(deadline.remaining()));
        }
        receipt["reason"] = json!(if deadline.expired() {
            "deadline_expired"
        } else {
            "submission_uncertain"
        });
        let _ = crate::atomic_write_json(&receipt_path, &receipt);
        Ok(receipt)
    }
}

fn read_receipt(path: &Path) -> Result<Option<serde_json::Value>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 8192 {
        bail!("invalid strict receipt");
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        bail!("unsafe strict receipt");
    }
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        bail!("strict receipt too large");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    const PAYLOAD: &str = "Reply with exactly the single word ACKDONE and nothing else.";
    fn screen(runtime: &WorkerRuntime, name: &str) {
        let plain = fs::read_to_string(format!(
            "{}/tests/fixtures/screens/codex/{name}.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        runtime
            .output
            .append(format!("\x1b[?2004h\x1b[2J\x1b[H{}", plain.replace('\n', "\r\n")).as_bytes())
            .unwrap();
    }
    fn fixture() -> (tempfile::TempDir, Arc<WorkerRuntime>, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let runtime =
            super::super::runtime::tests::test_runtime(&dir, dir.path().join("session.json"));
        {
            let mut record = runtime.record.lock().unwrap();
            record.engine = "codex".into();
            record.phase = Phase::Running;
            record.created_at_ms = crate::now_ms();
        }
        let writes = dir.path().join("input");
        *runtime.pty_write.lock().unwrap() = Some(Arc::new(
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&writes)
                .unwrap(),
        ));
        crate::ensure_private_dir(&runtime.paths.state_session(runtime.id)).unwrap();
        screen(&runtime, "ready");
        (dir, Arc::new(runtime), writes)
    }
    fn submit(runtime: &WorkerRuntime, id: Uuid, text: &str) -> serde_json::Value {
        let record = runtime.record().unwrap();
        runtime
            .submit_strict(
                id,
                &record.tag,
                &record.workspace,
                record.created_at_ms,
                record.created_at_ms + 60_000,
                text,
            )
            .unwrap()
    }

    fn submit_before(runtime: &WorkerRuntime, id: Uuid, deadline_ms: u64) -> serde_json::Value {
        let record = runtime.record().unwrap();
        runtime
            .submit_strict(
                id,
                &record.tag,
                &record.workspace,
                record.created_at_ms,
                deadline_ms,
                PAYLOAD,
            )
            .unwrap()
    }

    #[test]
    fn strict_expired_request_refuses_before_any_fence_or_input() {
        let (_dir, runtime, writes) = fixture();
        let operation = Uuid::new_v4();
        let receipt = submit_before(&runtime, operation, crate::now_ms());
        assert_eq!(receipt["status"], "refused");
        assert_eq!(receipt["reason"], "deadline_expired");
        assert_eq!(receipt["mutation"], false);
        assert_eq!(fs::metadata(writes).unwrap().len(), 0);
        assert!(!runtime
            .paths
            .state_session(runtime.id)
            .join(format!("strict-send-{operation}.json"))
            .exists());
    }

    #[test]
    fn strict_slow_durable_fence_and_pre_paste_delay_cannot_write_after_deadline() {
        for stage in ["fsync", "paste"] {
            let (_dir, runtime, writes) = fixture();
            let operation = Uuid::new_v4();
            let deadline = crate::now_ms() + 100;
            STAGE_DELAY.with(|delay| delay.set(Some((stage, Duration::from_millis(150)))));
            let receipt = submit_before(&runtime, operation, deadline);
            assert_eq!(receipt["status"], "uncertain", "{stage}: {receipt}");
            assert_eq!(receipt["reason"], "deadline_expired");
            assert_eq!(receipt["deadline_ms"], deadline);
            assert_eq!(fs::metadata(&writes).unwrap().len(), 0);
            // The durable uncertain fence stays binding even after expiry.
            assert_eq!(
                submit_before(&runtime, operation, deadline)["replayed"],
                true
            );
            assert_eq!(
                submit_before(&runtime, operation, deadline + 1)["reason"],
                "operation_conflict"
            );
            assert_eq!(fs::metadata(writes).unwrap().len(), 0);
        }
    }

    #[test]
    fn strict_deadline_after_landing_prevents_enter() {
        let (_dir, runtime, writes) = fixture();
        let worker = runtime.clone();
        let input = writes.clone();
        let paint = thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until && fs::metadata(&input).unwrap().len() == 0 {
                thread::sleep(Duration::from_millis(1));
            }
            screen(&worker, "pending-short");
        });
        let deadline = crate::now_ms() + 300;
        STAGE_DELAY.with(|delay| delay.set(Some(("enter", Duration::from_millis(350)))));
        let receipt = submit_before(&runtime, Uuid::new_v4(), deadline);
        paint.join().unwrap();
        assert_eq!(receipt["reason"], "deadline_expired");
        assert_eq!(receipt["landed"], true);
        assert_eq!(receipt["submitted"], false);
        assert_eq!(
            fs::read(writes).unwrap(),
            format!("\x1b[200~{PAYLOAD}\x1b[201~").as_bytes()
        );
    }
    #[test]
    fn strict_refusals_write_zero_bytes_for_identity_unknown_busy_and_drafts() {
        let (_dir, runtime, writes) = fixture();
        let r = runtime.record().unwrap();
        assert_eq!(
            runtime
                .submit_strict(
                    Uuid::new_v4(),
                    "wrong",
                    &r.workspace,
                    r.created_at_ms,
                    r.created_at_ms + 60_000,
                    PAYLOAD
                )
                .unwrap()["reason"],
            "identity_mismatch"
        );
        assert_eq!(
            runtime
                .submit_strict(
                    Uuid::new_v4(),
                    &r.tag,
                    Path::new("/different"),
                    r.created_at_ms,
                    r.created_at_ms + 60_000,
                    PAYLOAD
                )
                .unwrap()["status"],
            "refused"
        );
        for layout in ["busy", "pending-short", "boot"] {
            screen(&runtime, layout);
            let receipt = submit(&runtime, Uuid::new_v4(), PAYLOAD);
            assert_eq!(receipt["status"], "refused", "{layout}: {receipt}");
            assert_eq!(receipt["mutation"], false);
        }
        runtime
            .output
            .append(b"\x1b[2J\x1b[Hunknown program")
            .unwrap();
        assert_eq!(
            submit(&runtime, Uuid::new_v4(), PAYLOAD)["reason"],
            "unknown_layout"
        );
        assert_eq!(fs::metadata(writes).unwrap().len(), 0);
    }
    #[test]
    fn strict_input_lease_excludes_raw_input_attach_rename_and_parallel_submission() {
        let (_dir, runtime, writes) = fixture();
        let _lease = runtime.input_gate.lock().unwrap();
        assert!(runtime.send(b"human draft").is_err());
        assert!(runtime.send_from_client(1, b"x").is_err());
        assert!(runtime
            .attach_client(AttachPayload::Tail(None), None, false)
            .is_err());
        let r = runtime.record().unwrap();
        assert!(runtime.rename(r.workspace, "changed".into()).is_err());
        assert_eq!(
            submit(&runtime, Uuid::new_v4(), PAYLOAD)["status"],
            "uncertain"
        );
        assert!(runtime.record().is_ok());
        assert_eq!(fs::metadata(writes).unwrap().len(), 0);
    }
    #[test]
    fn strict_submission_is_verified_once_and_replay_never_writes_again() {
        let (_dir, runtime, writes) = fixture();
        let worker = runtime.clone();
        let input = writes.clone();
        let paint = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && fs::metadata(&input).unwrap().len() == 0 {
                thread::sleep(Duration::from_millis(5));
            }
            screen(&worker, "pending-short");
            while Instant::now() < deadline && !fs::read(&input).unwrap().ends_with(b"\r") {
                thread::sleep(Duration::from_millis(5));
            }
            screen(&worker, "done");
        });
        let operation = Uuid::new_v4();
        let receipt = submit(&runtime, operation, PAYLOAD);
        paint.join().unwrap();
        assert_eq!(receipt["status"], "submitted_verified", "{receipt}");
        assert_eq!(receipt["submitted"], true);
        assert!(receipt["consumed"].is_null());
        let bytes = fs::read(&writes).unwrap();
        assert_eq!(bytes, format!("\x1b[200~{PAYLOAD}\x1b[201~\r").as_bytes());
        assert_eq!(submit(&runtime, operation, PAYLOAD)["replayed"], true);
        assert_eq!(
            submit(&runtime, operation, "different")["reason"],
            "operation_conflict"
        );
        assert_eq!(fs::read(writes).unwrap(), bytes);
    }
    #[test]
    fn strict_cannot_interleave_with_ordinary_paste_or_use_a_pre_input_screen() {
        let (_dir, runtime, writes) = fixture();
        // The opening frame is split, just as a raw IPC writer may split it.
        runtime.send(b"\x1b[2").unwrap();
        runtime.send(b"00~").unwrap();
        assert_eq!(
            submit(&runtime, Uuid::new_v4(), PAYLOAD)["reason"],
            "input_not_settled"
        );
        runtime.send(b"body\x1b[201~").unwrap();
        {
            runtime.input_safety.lock().unwrap().last_write =
                Some(Instant::now() - Duration::from_secs(1));
        }
        // Even after quiet, readiness rendered before the send is insufficient.
        let before = fs::read(&writes).unwrap();
        assert_eq!(
            submit(&runtime, Uuid::new_v4(), PAYLOAD)["reason"],
            "input_not_settled"
        );
        assert_eq!(fs::read(&writes).unwrap(), before);
        screen(&runtime, "pending-short");
        runtime.last_activity_ms.store(1, Ordering::Relaxed);
        assert_eq!(
            submit(&runtime, Uuid::new_v4(), PAYLOAD)["status"],
            "refused"
        );
        assert_eq!(fs::read(writes).unwrap(), before);
    }

    #[test]
    fn strict_uncertain_write_blocks_new_operation_ids_on_the_same_stale_screen() {
        let (_dir, runtime, writes) = fixture();
        let first = submit(&runtime, Uuid::new_v4(), PAYLOAD);
        assert_eq!(first["reason"], "landing_uncertain");
        let bytes = fs::read(&writes).unwrap();
        let second = submit(&runtime, Uuid::new_v4(), "new operation must not paste");
        assert_eq!(second["reason"], "input_not_settled");
        assert_eq!(second["mutation"], false);
        assert_eq!(fs::read(&writes).unwrap(), bytes);
        runtime.input_safety.lock().unwrap().last_write =
            Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            submit(&runtime, Uuid::new_v4(), "after quiet, still stale")["reason"],
            "input_not_settled"
        );
        assert_eq!(fs::read(writes).unwrap(), bytes);
    }

    #[test]
    fn strict_delayed_preinput_activity_store_cannot_unlock_stale_rendering() {
        let (_dir, runtime, writes) = fixture();
        screen(&runtime, "ready");
        runtime.send(b"ordinary input").unwrap();
        runtime.input_safety.lock().unwrap().last_write =
            Some(Instant::now() - Duration::from_secs(1));
        runtime.last_activity_ms.store(u64::MAX, Ordering::Relaxed);
        let bytes = fs::read(&writes).unwrap();
        assert_eq!(
            submit(&runtime, Uuid::new_v4(), PAYLOAD)["reason"],
            "input_not_settled"
        );
        assert_eq!(fs::read(writes).unwrap(), bytes);
    }

    #[test]
    fn strict_render_generation_is_atomic_with_screen_not_delayed_activity_timestamp() {
        let (_dir, runtime, _writes) = fixture();
        // Simulate append before input and the reader's activity store after it.
        screen(&runtime, "ready");
        runtime.send(b"ordinary input").unwrap();
        runtime.input_safety.lock().unwrap().last_write =
            Some(Instant::now() - Duration::from_secs(1));
        runtime.last_activity_ms.store(u64::MAX, Ordering::Relaxed);
        let (_, _, before_generation) = runtime.output.strict_input_snapshot().unwrap();
        assert!(!runtime.input_is_settled_at(before_generation).unwrap());
        screen(&runtime, "ready");
        let (plain, _, after_generation) = runtime.output.strict_input_snapshot().unwrap();
        assert!(after_generation > before_generation);
        assert!(runtime.input_is_settled_at(after_generation).unwrap());
        assert!(plain.contains("Ask Codex"));
    }

    #[test]
    fn strict_first_fence_uses_existing_durable_record_parent_before_input() {
        let (_dir, runtime, writes) = fixture();
        let operation = Uuid::new_v4();
        let parent = runtime.paths.state_session(runtime.id);
        crate::ensure_private_dir(&parent).unwrap();
        let inode = fs::metadata(&parent).unwrap().ino();
        let fence = parent.join(format!("strict-send-{operation}.json"));
        assert!(!parent.join("strict-send").exists());
        let observer = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline && fs::metadata(&writes).unwrap().len() == 0 {
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(fs::metadata(&parent).unwrap().ino(), inode);
            assert!(!parent.join("strict-send").exists());
            let receipt = read_receipt(&fence).unwrap().unwrap();
            assert_eq!(receipt["status"], "uncertain");
            assert_eq!(receipt["operation_id"], operation.to_string());
        });
        assert_eq!(submit(&runtime, operation, PAYLOAD)["status"], "uncertain");
        observer.join().unwrap();
    }

    #[test]
    fn strict_receipt_reader_rejects_fifo_before_any_blocking_read() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("receipt-fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || sender.send(read_receipt(&fifo).is_err()).unwrap());
        assert!(receiver
            .recv_timeout(Duration::from_millis(500))
            .expect("receipt FIFO open blocked"));
        worker.join().unwrap();
    }

    #[test]
    fn strict_fifo_receipt_is_refused_without_blocking_input_gate() {
        use std::os::unix::ffi::OsStrExt;
        let (_dir, runtime, writes) = fixture();
        let operation = Uuid::new_v4();
        let fifo = runtime
            .paths
            .state_session(runtime.id)
            .join(format!("strict-send-{operation}.json"));
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let worker = runtime.clone();
        let (sender, receiver) = mpsc::channel();
        let thread =
            thread::spawn(move || sender.send(submit(&worker, operation, PAYLOAD)).unwrap());
        let receipt = receiver
            .recv_timeout(Duration::from_millis(500))
            .expect("FIFO receipt open blocked");
        thread.join().unwrap();
        assert_eq!(receipt["reason"], "receipt_unreadable");
        assert!(runtime.input_gate.try_lock().is_ok());
        assert_eq!(fs::metadata(writes).unwrap().len(), 0);
    }

    #[test]
    fn strict_corrupt_or_symlinked_receipt_cannot_claim_delivery_or_trigger_input() {
        let (dir, runtime, writes) = fixture();
        let operation = Uuid::new_v4();
        let receipt = runtime
            .paths
            .state_session(runtime.id)
            .join(format!("strict-send-{operation}.json"));
        crate::ensure_private_dir(receipt.parent().unwrap()).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"private secret").unwrap();
        std::os::unix::fs::symlink(&outside, &receipt).unwrap();
        let result = submit(&runtime, operation, PAYLOAD);
        assert_eq!(result["status"], "uncertain");
        assert_eq!(result["reason"], "receipt_unreadable");
        assert!(!result.to_string().contains("private"));
        assert_eq!(fs::metadata(&writes).unwrap().len(), 0);
        fs::remove_file(&receipt).unwrap();
        fs::write(&receipt, b"not JSON").unwrap();
        assert_eq!(submit(&runtime, operation, PAYLOAD)["status"], "uncertain");
        assert_eq!(fs::metadata(writes).unwrap().len(), 0);
    }

    #[test]
    fn strict_lost_landing_retains_uncertainty_and_recovered_fence_prevents_resend() {
        let (_dir, runtime, writes) = fixture();
        let operation = Uuid::new_v4();
        let receipt = submit(&runtime, operation, PAYLOAD);
        assert_eq!(receipt["status"], "uncertain");
        assert_eq!(receipt["reason"], "landing_uncertain");
        let bytes = fs::read(&writes).unwrap();
        assert!(!bytes.ends_with(b"\r"));
        // Load from disk, not an in-memory receipt cache: this is the same
        // path a replacement worker uses after crashing mid-dispatch.
        assert_eq!(submit(&runtime, operation, PAYLOAD)["replayed"], true);
        assert_eq!(fs::read(&writes).unwrap(), bytes);
        let dir = _dir;
        let mut recovered =
            super::super::runtime::tests::test_runtime(&dir, dir.path().join("session.json"));
        recovered.id = runtime.id;
        *recovered.record.lock().unwrap() = runtime.record().unwrap();
        screen(&recovered, "ready");
        assert_eq!(submit(&recovered, operation, PAYLOAD)["replayed"], true);
        assert_eq!(fs::read(writes).unwrap(), bytes);
    }
}
