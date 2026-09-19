use super::*;
use crate::record::ExitInfo;
use crate::warnings::{
    acknowledge_warnings, load_warning_for, load_warnings, record_warning, select_warnings,
    sweep_warnings, warning_for_record, WarningKind,
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
    let record = SessionRecord::fixture(PathBuf::from("/tmp/aplexer-warnings-test"), "boom");

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
    let mut other = SessionRecord::fixture(PathBuf::from("/elsewhere"), "boom");
    other.created_at_ms = 1;
    let record = SessionRecord::fixture(ws.clone(), "boom");
    record_warning(&paths, &record).unwrap();
    record_warning(&paths, &other).unwrap();

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
