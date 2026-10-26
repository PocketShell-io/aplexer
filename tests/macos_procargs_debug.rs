//! TEMPORARY diagnostic: dump KERN_PROCARGS2 parsing on a real Mac.
//! Deleted after the layout is confirmed.

#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
fn dump_procargs_layout() {
    let me = std::process::id();
    eprintln!("self pid={me}");
    eprintln!(
        "self argv={:?}",
        aplexer::sys::macos::proc_argv_of(me)
    );
    eprintln!(
        "self environ APLEXER_SESSION_ID={:?}",
        aplexer::sys::macos::proc_environ_bytes(me)
            .and_then(|b| aplexer::sys::macos::environ_lookup(&b, "APLEXER_SESSION_ID"))
    );
    let mut child = Command::new("/bin/sh")
        .args(["-c", "sleep 5"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.id();
    eprintln!("child pid={pid}");
    eprintln!("child ppid={:?}", aplexer::sys::macos::proc_ppid(pid));
    eprintln!("child argv={:?}", aplexer::sys::macos::proc_argv_of(pid));
    eprintln!(
        "child name={:?}",
        aplexer::sys::macos::proc_name_of(pid)
    );
    eprintln!(
        "children of self contains child: {:?}",
        aplexer::sys::macos::direct_children_of(me).contains(&pid)
    );
    child.kill().unwrap();
    child.wait().unwrap();
}
