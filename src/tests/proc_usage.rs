//! Process-tree accounting: who owns a pid, and when two samples become a rate.

use super::*;
use crate::proc_usage::{
    bracket_session_proc_usage, cached_session_proc_usage, clock_ticks_hz, cpu_percent, mono_ms,
    scan_session_procs, SessionProcUsage,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn write_stat(
    proc_root: &Path,
    pid: u32,
    comm: &str,
    ppid: u32,
    utime: u64,
    stime: u64,
    start: u64,
) {
    let dir = proc_root.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    // Field order after comm matches proc(5): state, ppid, then nine
    // ignored fields, utime, stime, six ignored fields, starttime.
    let line =
        format!("{pid} ({comm}) S {ppid} 0 0 0 0 0 0 0 0 0 {utime} {stime} 0 0 20 0 1 0 {start}\n");
    fs::write(dir.join("stat"), line).unwrap();
}

fn temp_proc() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // Match a real /proc: the cache writer refuses a parent it does not own
    // exclusively, so the runtime dir used as the cache parent is private.
    let runtime = dir.path().join("run");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    (dir, runtime)
}

#[test]
fn cpu_percent_is_one_core_when_the_tree_burns_hz_jiffies_per_second() {
    assert_eq!(cpu_percent(0, 100, 1000, 100), Some(100.0));
    assert_eq!(cpu_percent(50, 250, 500, 100), Some(400.0));
    assert_eq!(cpu_percent(10, 9, 1000, 100), None);
    assert_eq!(cpu_percent(0, 1, 0, 100), None);
}

#[test]
fn stat_comm_may_contain_parentheses_and_spaces() {
    let (dir, _) = temp_proc();
    let proc_root = dir.path().join("proc");
    write_stat(&proc_root, 10, "a) b", 1, 5, 7, 42);
    write_stat(&proc_root, 1, "init", 0, 0, 0, 1);
    let id = Uuid::new_v4();
    let snap = scan_session_procs(&proc_root, &[(id, 10)]);
    let row = &snap.rows[&id];
    assert_eq!(row.processes, 1);
    assert_eq!(row.jiffies, 12);
    assert_eq!(row.start_ticks, 42);
}

#[test]
fn nested_session_keeps_its_own_processes() {
    let (dir, _) = temp_proc();
    let proc_root = dir.path().join("proc");
    write_stat(&proc_root, 1, "init", 0, 0, 0, 1);
    write_stat(&proc_root, 10, "outer", 1, 1, 0, 100);
    write_stat(&proc_root, 30, "tool", 10, 4, 0, 110);
    write_stat(&proc_root, 20, "inner", 10, 2, 0, 120);
    write_stat(&proc_root, 21, "test", 20, 8, 0, 130);
    write_stat(&proc_root, 99, "other", 1, 50, 0, 140);
    let outer = Uuid::new_v4();
    let inner = Uuid::new_v4();
    let snap = scan_session_procs(&proc_root, &[(outer, 10), (inner, 20)]);
    assert_eq!(
        snap.rows[&outer].processes, 2,
        "outer worker plus its own child"
    );
    assert_eq!(snap.rows[&outer].jiffies, 5);
    assert_eq!(snap.rows[&inner].processes, 2);
    assert_eq!(snap.rows[&inner].jiffies, 10);
}

#[test]
fn missing_worker_counts_as_an_empty_tree() {
    let (dir, _) = temp_proc();
    let proc_root = dir.path().join("proc");
    fs::create_dir_all(&proc_root).unwrap();
    let id = Uuid::new_v4();
    let snap = scan_session_procs(&proc_root, &[(id, 10)]);
    assert_eq!(snap.rows[&id].processes, 0);
    assert_eq!(snap.rows[&id].start_ticks, 0);
}

#[test]
fn cache_turns_a_one_second_old_sample_into_a_rate_and_ignores_a_stale_one() {
    let (dir, runtime) = temp_proc();
    let proc_root = dir.path().join("proc");
    write_stat(&proc_root, 10, "worker", 1, 200, 0, 1000);
    write_stat(&proc_root, 1, "init", 0, 0, 0, 1);
    let id = Uuid::new_v4();
    let cache = runtime.join("proc-usage.json");
    let now = mono_ms();
    let fresh = format!(
        r#"{{"mono_ms":{},"sessions":{{"{id}":{{"pid":10,"start_ticks":1000,"jiffies":100,"processes":1,"cpu_percent":null}}}}}}"#,
        now.saturating_sub(1000)
    );
    fs::write(&cache, fresh).unwrap();
    let usage = cached_session_proc_usage(&proc_root, &cache, &[(id, 10)]);
    let sample = usage[&id];
    assert_eq!(sample.processes, 1);
    let cpu = sample.cpu_percent.expect("fresh cache has a rate");
    // 100 jiffies over about 1s. The call's own clock moves a few
    // milliseconds, so the rate only has to land near one core's worth.
    let expected = 100.0 / clock_ticks_hz() as f64 * 100.0;
    assert!(
        (cpu - expected).abs() < expected * 0.05 + 1.0,
        "{cpu} vs {expected}"
    );

    let stale = format!(
        r#"{{"mono_ms":{},"sessions":{{"{id}":{{"pid":10,"start_ticks":1000,"jiffies":100,"processes":1,"cpu_percent":80.0}}}}}}"#,
        now.saturating_sub(60_000)
    );
    fs::write(&cache, stale).unwrap();
    let usage = cached_session_proc_usage(&proc_root, &cache, &[(id, 10)]);
    assert_eq!(usage[&id].processes, 1);
    assert_eq!(usage[&id].cpu_percent, None);
}

#[test]
fn a_sample_younger_than_the_rate_window_keeps_the_stored_rate() {
    let (dir, runtime) = temp_proc();
    let proc_root = dir.path().join("proc");
    write_stat(&proc_root, 10, "worker", 1, 5, 0, 1000);
    write_stat(&proc_root, 1, "init", 0, 0, 0, 1);
    let id = Uuid::new_v4();
    let cache = runtime.join("proc-usage.json");
    let now = mono_ms();
    let body = format!(
        r#"{{"mono_ms":{},"sessions":{{"{id}":{{"pid":10,"start_ticks":1000,"jiffies":5,"processes":1,"cpu_percent":250.0}}}}}}"#,
        now.saturating_sub(50)
    );
    fs::write(&cache, body).unwrap();
    let usage = cached_session_proc_usage(&proc_root, &cache, &[(id, 10)]);
    assert_eq!(usage[&id].cpu_percent, Some(250.0));
}

#[test]
fn pid_reuse_does_not_inherit_the_previous_process_rate() {
    let (dir, runtime) = temp_proc();
    let proc_root = dir.path().join("proc");
    write_stat(&proc_root, 10, "worker", 1, 500, 0, 9999);
    write_stat(&proc_root, 1, "init", 0, 0, 0, 1);
    let id = Uuid::new_v4();
    let cache = runtime.join("proc-usage.json");
    let now = mono_ms();
    let body = format!(
        r#"{{"mono_ms":{},"sessions":{{"{id}":{{"pid":10,"start_ticks":1000,"jiffies":0,"processes":1,"cpu_percent":900.0}}}}}}"#,
        now.saturating_sub(1000)
    );
    fs::write(&cache, body).unwrap();
    let usage = cached_session_proc_usage(&proc_root, &cache, &[(id, 10)]);
    assert_eq!(usage[&id].cpu_percent, None);
    assert_eq!(usage[&id].processes, 1);
}

#[test]
fn bracket_with_no_roots_does_not_sleep() {
    let (dir, runtime) = temp_proc();
    let first = scan_session_procs(dir.path(), &[]);
    let started = Instant::now();
    let usage = bracket_session_proc_usage(
        dir.path(),
        &runtime.join("proc-usage.json"),
        &first,
        &[],
        Duration::from_secs(5),
    );
    assert!(usage.is_empty());
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[test]
fn list_suffix_hides_an_empty_tree_and_idle_noise() {
    assert_eq!(
        SessionProcUsage {
            processes: 0,
            cpu_percent: Some(400.0)
        }
        .list_suffix(),
        ""
    );
    assert_eq!(
        SessionProcUsage {
            processes: 3,
            cpu_percent: Some(4.0)
        }
        .list_suffix(),
        "3p"
    );
    assert_eq!(
        SessionProcUsage {
            processes: 17,
            cpu_percent: Some(339.6)
        }
        .list_suffix(),
        "17p 340%"
    );
    assert_eq!(
        SessionProcUsage {
            processes: 17,
            cpu_percent: None
        }
        .detail(),
        "17 processes"
    );
    assert_eq!(
        SessionProcUsage {
            processes: 17,
            cpu_percent: Some(80.0)
        }
        .detail(),
        "17 processes · 80% cpu"
    );
}
