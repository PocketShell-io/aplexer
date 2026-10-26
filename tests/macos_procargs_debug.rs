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
    let argv = aplexer::sys::macos::proc_argv_of(pid)
        .expect("argv must be readable");
    assert_eq!(
        argv,
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string()
        ],
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
