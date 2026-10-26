//! libproc enumeration semantics: `proc_list*` returns a pid COUNT.

#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
fn proc_list_returns_counts_and_children_resolve() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "sleep 5"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.id();
    // `sh -c` with a single command execs it directly.
    assert_eq!(
        aplexer::sys::macos::proc_argv_of(pid),
        Some(vec!["sleep".to_string(), "5".to_string()])
    );
    assert!(aplexer::sys::macos::all_pids().contains(&pid));
    assert!(
        aplexer::sys::macos::direct_children_of(std::process::id()).contains(&pid),
        "the spawned child must be listed as ours"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}
