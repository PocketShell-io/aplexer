use super::*;
use crate::record::ExitInfo;
use crate::warnings::{
    acknowledge_warnings, diagnostic_error_for_record, load_warning_for, load_warnings,
    record_warning, select_warnings, sweep_warnings, warning_for_record, WarningKind,
};

fn test_paths(root: &std::path::Path) -> Paths {
    let paths = Paths {
        runtime_root: root.join("runtime"),
        state_root: root.join("state"),
        config_file: root.join("config.toml"),
    };
    paths.ensure().unwrap();
    paths
}

fn persist_record(paths: &Paths, record: &SessionRecord) {
    fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), record).unwrap();
}

/// A fixture whose durable paths match what a real registry holds, so the
/// registry readers (`list_records`, `read_session_record`) accept it once
/// persisted.
fn registry_fixture(paths: &Paths, ws: PathBuf, tag: &str) -> SessionRecord {
    let mut record = SessionRecord::fixture(ws, tag);
    record.socket_path = paths.socket(record.id);
    record.history_path = paths.history(record.id);
    record
}

fn oom_exit() -> ExitInfo {
    ExitInfo {
        code: Some(137),
        signal: Some(9),
        oom_killed: true,
        exited_at_ms: crate::util::now_ms(),
    }
}

#[test]
fn predicate_flags_oom_broken_and_failed_but_not_a_plain_exit() {
    let ws = PathBuf::from("/tmp/aplexer-warnings-test");
    // OOM kill is the kernel's own diagnosis and wins over everything else.
    let mut oom = SessionRecord::fixture(ws.clone(), "oomed");
    oom.phase = Phase::Exited;
    oom.exit = Some(oom_exit());
    let (kind, detail) = warning_for_record(&oom).unwrap();
    assert_eq!(kind, WarningKind::Oom);
    assert!(detail.contains("OOM killer"), "{detail}");

    // A running record whose worker is gone (the fixture has no live pid)
    // and whose startup window is long past: the dead-worker crash.
    let broken = SessionRecord::fixture(ws.clone(), "broken");
    let (kind, detail) = warning_for_record(&broken).unwrap();
    assert_eq!(kind, WarningKind::Crash);
    assert!(detail.contains("no exit recorded"), "{detail}");
    assert!(
        detail.contains("termination signal and kill source are unknown"),
        "{detail}"
    );
    assert_eq!(
        diagnostic_error_for_record(&broken).as_deref(),
        Some(detail.as_str()),
        "retirement must preserve the same diagnosis shown by `a warnings`"
    );

    // A worker that lived to record its own failure.
    let mut failed = SessionRecord::fixture(ws.clone(), "failed");
    failed.phase = Phase::Failed;
    failed.error = Some("agent exited with failure".into());
    let (kind, detail) = warning_for_record(&failed).unwrap();
    assert_eq!(kind, WarningKind::Crash);
    assert!(detail.contains("agent exited with failure"), "{detail}");

    // A session that merely ended -- including `a kill`'s signal -- warns
    // nothing.
    let mut exited = SessionRecord::fixture(ws, "done");
    exited.phase = Phase::Exited;
    exited.exit = Some(ExitInfo {
        code: Some(0),
        signal: Some(15),
        oom_killed: false,
        exited_at_ms: crate::util::now_ms(),
    });
    assert!(warning_for_record(&exited).is_none());
}

#[test]
fn warning_is_write_once_and_acknowledge_removes_it() {
    let root = tempfile::tempdir().unwrap();
    let paths = test_paths(root.path());
    let record = registry_fixture(&paths, PathBuf::from("/tmp/aplexer-warnings-test"), "boom");
    persist_record(&paths, &record);

    assert!(record_warning(&paths, &record).unwrap());
    let first = load_warning_for(&paths, record.id).unwrap();
    assert_eq!(first.session, record.id);
    assert_eq!(first.tag, "boom");

    // Rewrite must not re-date or duplicate: write-if-absent.
    assert!(!record_warning(&paths, &record).unwrap());
    assert_eq!(
        load_warning_for(&paths, record.id).unwrap().created_at_ms,
        first.created_at_ms
    );

    assert_eq!(
        acknowledge_warnings(&paths, &[record.id]).unwrap(),
        vec![record.id]
    );
    // Acknowledging twice is fine -- the goal state is "no warning".
    assert!(acknowledge_warnings(&paths, &[record.id])
        .unwrap()
        .is_empty());
    assert!(load_warning_for(&paths, record.id).is_none());
    assert!(load_warnings(&paths).is_empty());
}

#[test]
fn sweep_materializes_warnings_from_registry_records() {
    let root = tempfile::tempdir().unwrap();
    let paths = test_paths(root.path());
    let mut record = SessionRecord::fixture(PathBuf::from("/tmp/aplexer-warnings-test"), "dead");
    // list_records validates each entry it yields (durable paths
    // included), so the persisted record must be one a real registry
    // would hold.
    record.socket_path = paths.socket(record.id);
    record.history_path = paths.history(record.id);
    persist_record(&paths, &record);

    assert_eq!(sweep_warnings(&paths), 1);
    assert_eq!(sweep_warnings(&paths), 0, "second sweep finds nothing new");
    assert!(load_warning_for(&paths, record.id).is_some());

    // The ack sticks even though the broken record itself still sits in
    // the registry: the sweep must not re-materialize an acknowledged
    // crash, or `a ack` could never clear anything before `a prune` ran.
    acknowledge_warnings(&paths, &[record.id]).unwrap();
    assert_eq!(
        sweep_warnings(&paths),
        0,
        "an acknowledged warning must stay acknowledged"
    );

    // A healthy registry warns nothing.
    record.phase = Phase::Exited;
    record.exit = Some(ExitInfo {
        code: Some(0),
        signal: None,
        oom_killed: false,
        exited_at_ms: crate::util::now_ms(),
    });
    persist_record(&paths, &record);
    assert_eq!(sweep_warnings(&paths), 0);
}

#[test]
fn selection_matches_ws_tag_uuid_prefix_and_cwd_tag() {
    let root = tempfile::tempdir().unwrap();
    let paths = test_paths(root.path());
    let ws = PathBuf::from("/tmp/aplexer-warnings-test");
    let mut other = registry_fixture(&paths, PathBuf::from("/elsewhere"), "boom");
    other.created_at_ms = 1;
    let record = registry_fixture(&paths, ws.clone(), "boom");
    persist_record(&paths, &record);
    persist_record(&paths, &other);
    assert!(record_warning(&paths, &record).unwrap());
    assert!(record_warning(&paths, &other).unwrap());

    let cwd = Some(ws.as_path());
    // workspace:tag pair
    assert_eq!(
        select_warnings(&paths, Some(" /tmp/nowhere:never"), None, None, cwd).len(),
        0
    );
    assert_eq!(
        select_warnings(
            &paths,
            Some(&format!("{}:boom", ws.display())),
            None,
            None,
            None
        )
        .iter()
        .map(|w| w.session)
        .collect::<Vec<_>>(),
        vec![record.id]
    );
    // UUID prefix, even though no registry record exists anymore
    let prefix = record.id.to_string()[..8].to_string();
    assert_eq!(
        select_warnings(&paths, Some(&prefix), None, None, None)[0].session,
        record.id
    );
    // Plain word = tag in the current workspace only
    assert_eq!(
        select_warnings(&paths, Some("boom"), None, None, cwd)[0].session,
        record.id
    );
    assert_eq!(
        select_warnings(&paths, Some("boom"), None, None, None).len(),
        2
    );
    // --tag/--workspace form
    assert_eq!(
        select_warnings(
            &paths,
            None,
            Some(std::path::Path::new("/elsewhere")),
            Some("boom"),
            None
        )[0]
        .session,
        other.id
    );
    // No selector at all: everything.
    assert_eq!(select_warnings(&paths, None, None, None, None).len(), 2);
}

#[test]
fn exiting_corpse_is_finalized_with_an_exit_record_not_reported_as_a_crash() {
    let root = tempfile::tempdir().unwrap();
    let paths = test_paths(root.path());
    let mut record = SessionRecord::fixture(PathBuf::from("/tmp/aplexer-warnings-test"), "stopped");
    record.socket_path = paths.socket(record.id);
    record.history_path = paths.history(record.id);
    // `a kill`/Hub stop accepted, worker torn down with the scope before it
    // could finalize: Exiting, no live worker or leader, no exit record.
    record.phase = Phase::Exiting;
    persist_record(&paths, &record);

    assert!(warning_for_record(&record).is_none(), "not a crash");
    assert_eq!(sweep_warnings(&paths), 0);
    assert!(load_warnings(&paths).is_empty());

    let stored = crate::registry::read_record(&paths.record(record.id)).unwrap();
    assert_eq!(stored.phase, Phase::Exited);
    assert!(stored.exit.is_some(), "exit record written");
    assert_eq!(stored.containment_empty, Some(false), "no false proof");
    assert!(!stored.containment_proven_empty());

    // A dead worker in any other active phase is still a real crash.
    let mut running = SessionRecord::fixture(PathBuf::from("/tmp/aplexer-warnings-test"), "died");
    running.phase = Phase::Running;
    assert!(warning_for_record(&running).is_some());
}

/// The false "worker died unexpectedly while exiting (no exit recorded)"
/// crash (Hub intake 33fde3fb): a sweep -- or the resident RPC registry's
/// cached records -- holds a snapshot read while the worker was still
/// finalizing (`phase: exiting`, no exit), and probes liveness only after
/// that worker removed its record, left a finished tombstone, and exited.
/// The snapshot alone looks `broken`; the warning write must re-read the
/// registry and see that the session ended normally.
#[test]
fn exiting_snapshot_of_a_cleanly_finalized_session_warns_nothing() {
    let root = tempfile::tempdir().unwrap();
    let paths = test_paths(root.path());
    let ws = PathBuf::from("/tmp/aplexer-warnings-test");

    // Fast-path finalization: record removed, worker gone.
    let mut removed = registry_fixture(&paths, ws.clone(), "hub-stopped");
    removed.phase = Phase::Running;
    persist_record(&paths, &removed);
    let stale_snapshot = removed.clone();
    fs::remove_dir_all(paths.state_session(removed.id)).unwrap();
    assert!(
        warning_for_record(&stale_snapshot).is_some(),
        "precondition: the stale snapshot alone reads as broken"
    );
    assert!(!record_warning(&paths, &stale_snapshot).unwrap());
    assert!(load_warning_for(&paths, removed.id).is_none());

    // Kept-record finalization (`keep_exited`, retained stop): the worker
    // rewrote its record as Exited with a clean exit before exiting.
    let mut kept = registry_fixture(&paths, ws.clone(), "kept");
    kept.phase = Phase::Exiting;
    let stale_snapshot = kept.clone();
    kept.phase = Phase::Exited;
    kept.exit = Some(ExitInfo {
        code: Some(0),
        signal: None,
        oom_killed: false,
        exited_at_ms: crate::util::now_ms(),
    });
    persist_record(&paths, &kept);
    assert!(!record_warning(&paths, &stale_snapshot).unwrap());
    assert!(load_warning_for(&paths, kept.id).is_none());

    // Same race in the `running` phase (a workload that exits on its own
    // between snapshot and probe).
    let running = registry_fixture(&paths, ws, "ran-out");
    assert!(!record_warning(&paths, &running).unwrap());
    assert!(load_warnings(&paths).is_empty());
}

/// The re-read must not hide a real dead-worker crash: a worker that died
/// in the `running` phase (no stop was ever accepted) is still warned.
#[test]
fn worker_that_died_while_running_without_finalizing_still_warns() {
    let root = tempfile::tempdir().unwrap();
    let paths = test_paths(root.path());
    let record = registry_fixture(
        &paths,
        PathBuf::from("/tmp/aplexer-warnings-test"),
        "sigkilled",
    );
    persist_record(&paths, &record);

    assert!(record_warning(&paths, &record).unwrap());
    let warning = load_warning_for(&paths, record.id).unwrap();
    assert_eq!(warning.kind, WarningKind::Crash);
    assert!(
        warning.detail.contains("while running"),
        "{}",
        warning.detail
    );
    assert_eq!(sweep_warnings(&paths), 0, "already recorded");
}

/// Hub intake 8c03677f (2026-11-03): 94 sessions died together when
/// `user@1000.service` was SIGKILLed and systemd tore down the whole user
/// slice, plus one earlier death in a `supervision.service` stop timeout.
/// Every warning read "worker died unexpectedly ... kill source unknown"
/// and needed journal forensics to explain. A dead worker whose recorded
/// cgroup sits beneath a per-user manager that started *after* the session
/// must name that teardown instead.
fn teardown_fixture(cgroup: Option<&str>, created_at_ms: u64) -> SessionRecord {
    let mut record =
        SessionRecord::fixture(PathBuf::from("/tmp/aplexer-warnings-test"), "torn-down");
    record.worker_cgroup = cgroup.map(str::to_string);
    record.created_at_ms = created_at_ms;
    record.updated_at_ms = created_at_ms;
    record
}

// Pinned clocks for the attribution tests: the session started an hour
// before "now"; a manager boot is either an hour after the session
// (teardown) or a minute before it (predates the session).
const TEARDOWN_NOW: u64 = 1_800_000_000_000;
const TEARDOWN_CREATED: u64 = TEARDOWN_NOW - 3_600_000;
const TEARDOWN_BOOT_NEWER: u64 = TEARDOWN_CREATED + 3_600_000;
const TEARDOWN_BOOT_OLDER: u64 = TEARDOWN_CREATED - 60_000;

fn user_manager_cgroup() -> String {
    "/user.slice/user-1000.slice/user@1000.service/app.slice/aplexer-workload-x.scope".to_string()
}

#[test]
fn manager_teardown_is_attributed_not_hidden() {
    let record = teardown_fixture(Some(&user_manager_cgroup()), TEARDOWN_CREATED);
    let (kind, detail) =
        crate::warnings::warning_for_record_at(&record, TEARDOWN_NOW, Some(TEARDOWN_BOOT_NEWER))
            .expect("dead worker still warns");
    assert_eq!(kind, WarningKind::Crash, "teardown deaths stay crashes");
    assert!(detail.contains("while running"), "{detail}");
    assert!(detail.contains("no exit recorded"), "{detail}");
    assert!(
        detail.contains("per-user systemd manager user@1000.service"),
        "{detail}"
    );
    assert!(
        detail.contains("tearing down every session beneath it together"),
        "{detail}"
    );
}

#[test]
fn manager_teardown_attribution_preserves_worker_error_context() {
    let mut record = teardown_fixture(Some(&user_manager_cgroup()), TEARDOWN_CREATED);
    record.error = Some("workload OOM score adjusted".to_string());
    let (_, detail) =
        crate::warnings::warning_for_record_at(&record, TEARDOWN_NOW, Some(TEARDOWN_BOOT_NEWER))
            .expect("dead worker still warns");
    assert!(detail.contains("workload OOM score adjusted"), "{detail}");
    assert!(detail.contains("user@1000.service"), "{detail}");
}

#[test]
fn older_manager_boot_keeps_the_generic_diagnosis() {
    let record = teardown_fixture(Some(&user_manager_cgroup()), TEARDOWN_CREATED);
    let (_, detail) =
        crate::warnings::warning_for_record_at(&record, TEARDOWN_NOW, Some(TEARDOWN_BOOT_OLDER))
            .expect("dead worker still warns");
    assert!(
        detail.contains("termination signal and kill source are unknown"),
        "{detail}"
    );
    assert!(!detail.contains("per-user systemd manager"), "{detail}");
}

#[test]
fn manager_boot_within_skew_margin_stays_generic() {
    // A boot 1s after session start is clock truncation, not evidence of a
    // teardown (see MANAGER_BOOT_SKEW_MS): never attribute on it.
    let record = teardown_fixture(Some(&user_manager_cgroup()), TEARDOWN_CREATED);
    let (_, detail) = crate::warnings::warning_for_record_at(
        &record,
        TEARDOWN_NOW,
        Some(TEARDOWN_CREATED + 1_000),
    )
    .expect("dead worker still warns");
    assert!(!detail.contains("per-user systemd manager"), "{detail}");
}

#[test]
fn missing_or_zero_manager_boot_stays_generic() {
    let record = teardown_fixture(Some(&user_manager_cgroup()), TEARDOWN_CREATED);
    for boot in [None, Some(0)] {
        let (_, detail) = crate::warnings::warning_for_record_at(&record, TEARDOWN_NOW, boot)
            .expect("dead worker still warns");
        assert!(
            detail.contains("termination signal and kill source are unknown"),
            "{detail}"
        );
        assert!(!detail.contains("per-user systemd manager"), "{detail}");
    }
}

#[test]
fn teardown_attribution_requires_the_user_manager_placement() {
    // A login-session worker (survives `systemctl --user exit`) killed
    // while its manager is new died some other way: stay generic.
    let mut record = teardown_fixture(
        Some("/user.slice/user-1000.slice/session-8.scope"),
        TEARDOWN_CREATED,
    );
    assert_eq!(
        crate::placement::classify_cgroup_path(record.worker_cgroup.as_deref().unwrap()),
        crate::placement::CgroupPlacement::LoginSession
    );
    let (_, detail) =
        crate::warnings::warning_for_record_at(&record, TEARDOWN_NOW, Some(TEARDOWN_BOOT_NEWER))
            .expect("dead worker still warns");
    assert!(!detail.contains("per-user systemd manager"), "{detail}");

    // No recorded cgroup at all: nothing to attribute.
    record.worker_cgroup = None;
    let (_, detail) =
        crate::warnings::warning_for_record_at(&record, TEARDOWN_NOW, Some(TEARDOWN_BOOT_NEWER))
            .expect("dead worker still warns");
    assert!(!detail.contains("per-user systemd manager"), "{detail}");
}

#[test]
fn failed_phase_is_not_manager_attributed() {
    // A worker that lived to record its own failure was not torn down with
    // a manager: the Failed arm never attributes.
    let mut record = teardown_fixture(Some(&user_manager_cgroup()), TEARDOWN_CREATED);
    record.phase = Phase::Failed;
    record.error = Some("agent exited with failure".to_string());
    let (_, detail) =
        crate::warnings::warning_for_record_at(&record, TEARDOWN_NOW, Some(TEARDOWN_BOOT_NEWER))
            .expect("failed worker still warns");
    assert!(detail.contains("agent exited with failure"), "{detail}");
    assert!(!detail.contains("per-user systemd manager"), "{detail}");
}

/// The live lookup behind the attribution: returns the manager's boot or
/// `None` (no systemd, no such user, wedged bus) without ever failing
/// hard. A returned boot must be sane: after 2020 and not in the future.
#[cfg(target_os = "linux")]
#[test]
fn live_user_manager_lookup_is_sane_or_absent() {
    let uid = unsafe { libc::getuid() };
    match crate::user_manager_started_ms(uid) {
        None => {}
        Some(boot_ms) => {
            assert!(boot_ms > 1_600_000_000_000, "boot {boot_ms} predates 2020");
            assert!(
                boot_ms <= crate::util::now_ms() + 60_000,
                "boot {boot_ms} is impossibly far in the future"
            );
        }
    }
}

/// End to end through the production predicate: a record that predates any
/// real manager boot is attributed exactly when the live lookup resolves
/// one, and stays generic otherwise. Deterministic on systemd and
/// non-systemd hosts alike because the session start (epoch 0) predates
/// every real boot past the skew margin.
#[cfg(target_os = "linux")]
#[test]
fn live_predicate_attributes_when_the_manager_postdates_the_session() {
    let uid = unsafe { libc::getuid() };
    let cgroup = format!("/user.slice/user-{uid}.slice/user@{uid}.service/app.slice");
    let record = teardown_fixture(Some(&cgroup), 0);
    let boot = crate::user_manager_started_ms(uid);
    let (kind, detail) = warning_for_record(&record).expect("dead worker still warns");
    assert_eq!(kind, WarningKind::Crash);
    match boot {
        Some(_) => assert!(
            detail.contains("per-user systemd manager"),
            "manager boot resolved but unattributed: {detail}"
        ),
        None => assert!(
            !detail.contains("per-user systemd manager"),
            "no manager boot but attributed: {detail}"
        ),
    }
}
