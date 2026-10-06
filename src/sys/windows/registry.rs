//! Minimal registry reads for `a doctor` (PATH model, PowerShell policy).

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hive {
    CurrentUser,
    LocalMachine,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Read a `REG_SZ`/`REG_EXPAND_SZ` value (expanded against this process's
/// environment). `None` when absent or unreadable.
pub fn read_string(hive: Hive, subkey: &str, value: &str) -> Option<String> {
    let root: HKEY = match hive {
        Hive::CurrentUser => HKEY_CURRENT_USER,
        Hive::LocalMachine => HKEY_LOCAL_MACHINE,
    };
    let (key, name) = (wide(subkey), wide(value));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut bytes = 0u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if rc != 0 || bytes == 0 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
    let mut bytes = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(
        OsString::from_wide(&buf[..len])
            .to_string_lossy()
            .into_owned(),
    )
}

/// Machine PATH then user PATH from the registry (what a fresh logon session
/// starts with), joined with `;`. `None` when neither is readable.
pub fn registry_path() -> Option<String> {
    let machine = read_string(
        Hive::LocalMachine,
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        "Path",
    );
    let user = read_string(Hive::CurrentUser, "Environment", "Path");
    let parts: Vec<String> = [machine, user]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(";"))
}

/// Configured Windows PowerShell 5.1 execution policy per scope as the
/// registry records it, most authoritative first: Group Policy (machine,
/// user), then CurrentUser, then LocalMachine.
pub fn powershell_execution_policies() -> Vec<(&'static str, String)> {
    let shell = r"Software\Microsoft\PowerShell\1\ShellIds\Microsoft.PowerShell";
    let gpo = r"Software\Policies\Microsoft\Windows\PowerShell";
    let mut out = Vec::new();
    for (scope, hive, key) in [
        ("MachinePolicy", Hive::LocalMachine, gpo),
        ("UserPolicy", Hive::CurrentUser, gpo),
        ("CurrentUser", Hive::CurrentUser, shell),
        ("LocalMachine", Hive::LocalMachine, shell),
    ] {
        if let Some(p) = read_string(hive, key, "ExecutionPolicy") {
            out.push((scope, p));
        }
    }
    out
}

/// Effective policy name (`Restricted`, the client default, when none is set)
/// plus the per-scope list.
pub fn effective_execution_policy() -> (String, Vec<(&'static str, String)>) {
    let list = powershell_execution_policies();
    let eff = list
        .first()
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| "Restricted".to_string());
    (eff, list)
}
