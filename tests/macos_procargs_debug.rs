//! TEMPORARY diagnostic: raw libproc return-value semantics.

#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
fn dump_procargs_layout() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "sleep 5"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.id() as libc::pid_t;
    let me = std::process::id() as libc::pid_t;

    // Raw proc_listchildpids call: is the return value a count or bytes?
    let mut buf = vec![0 as libc::c_int; 256];
    let r_child = unsafe {
        libc::proc_listchildpids(
            me,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            (buf.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    let as_count: Vec<i32> = buf.iter().take(r_child.max(0) as usize).copied().collect();
    let as_bytes: Vec<i32> = buf
        .iter()
        .take((r_child.max(0) as usize) / std::mem::size_of::<libc::c_int>())
        .copied()
        .collect();

    // Raw proc_listallpids call for comparison.
    let mut buf_all = vec![0 as libc::c_int; 262144];
    let r_all = unsafe {
        libc::proc_listallpids(
            buf_all.as_mut_ptr().cast::<libc::c_void>(),
            (buf_all.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };

    panic!(
        "childpid={pid} listchildpids(self)={r_child} as_count={as_count:?} as_bytes={as_bytes:?} listallpids={r_all} argv={:?}",
        aplexer::sys::macos::proc_argv_of(pid as u32)
    );
    #[allow(unreachable_code)]
    {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
