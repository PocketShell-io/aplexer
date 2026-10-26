//! TEMPORARY diagnostic: validates KERN_PROCARGS2 parsing on a real Mac.

#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
fn dump_procargs_layout() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "sleep 5"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.id();
    let argv = aplexer::sys::macos::proc_argv_of(pid).expect("argv must be readable");
    // `sh -c` with a single command execs it directly, so the child is
    // already `sleep 5` rather than `sh` -- the parse is what matters.
    assert_eq!(
        argv,
        vec!["sleep".to_string(), "5".to_string()],
        "unexpected argv layout"
    );
    let name = aplexer::sys::macos::proc_name_of(pid).expect("name must be readable");
    assert!(name.contains("sh"), "unexpected process name: {name}");
    assert!(
        aplexer::sys::macos::direct_children_of(std::process::id()).contains(&pid),
        "child must be listed as ours"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}
