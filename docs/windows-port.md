# Native Windows port: contract

Target: Windows 11 / Server 2022+ (ConPTY needs 1809+). MSVC toolchain, `x86_64-pc-windows-msvc`.
Unix behaviour must not change. Unix code stays in place behind `#[cfg(unix)]`
(or `target_os = "linux"` for cgroup/pidfd/`/proc`); Windows code lives in
`src/sys/windows/*` and is called from `#[cfg(windows)]` branches.

## Decisions
- **IPC**: byte-mode named pipes `\\.\pipe\aplexer-<user-sid>-<session-uuid>`, owner-only DACL,
  `FILE_FLAG_FIRST_PIPE_INSTANCE`, `PIPE_REJECT_REMOTE_CLIENTS`. Timeouts via overlapped I/O + `CancelIoEx`.
  The record's `socket_path` holds the pipe name on Windows. Existence probes become connect attempts.
- **PTY**: ConPTY via raw `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`. No slave fd.
  `PtyMaster` holds HPCON + input/output handles; resize via `ResizePseudoConsole`.
- **Containment/tree kill**: Job Object per session, `KILL_ON_JOB_CLOSE`, no breakaway. Replaces
  cgroups, pidfd, subreaper, SIGCHLD. Named job `aplexer-<uuid>` for reopen.
- **Identity**: `{pid, creation FILETIME}` from `GetProcessTimes`; boot id from boot time (or dropped).
- **Signals**: wire stays `i32`. Map TERM/INT -> write `0x03` to PTY input (graceful), KILL -> `TerminateJobObject`;
  HUP/QUIT/USR1/USR2 -> clear "unsupported on Windows" error. No `ExitStatus::signal()`.
- **Dirs**: state `%LOCALAPPDATA%\aplexer\state`, runtime `%LOCALAPPDATA%\aplexer\run`,
  config `%APPDATA%\aplexer\config.toml`; keep `APLEXER_*` overrides. Home via `USERPROFILE`.
- **Locks**: `LockFileEx` in place of `flock`. Atomic replace via `MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH)`;
  no dir fsync; no `RENAME_EXCHANGE`.
- **Shell default**: `pwsh.exe` -> `powershell.exe` -> `%COMSPEC%`, no `-l`.
- **Gated off on Windows v1** (`#[cfg(target_os = "linux")]`): `cgroup/*`, `placement`, systemd scopes,
  `startup-test-hooks`, subreaper/SIGCHLD.
- **Process inspection** (`sys/windows/procinfo.rs`) replaces `/proc` for cwd tracking, the
  foreground command and agent detection: a `CreateToolhelp32Snapshot` tree (pid, ppid, exe name)
  plus `NtQueryInformationProcess` + `ReadProcessMemory` of the PEB's `RTL_USER_PROCESS_PARAMETERS`
  (cwd, command line, environment; x64 and WOW64 targets via PEB32) opened with
  `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`. Any failure (exited pid, access denied,
  elevated or other-user process) yields `None`, never an error. Windows has no foreground process
  group, so the foreground command is the deepest, newest live process under the workload leader
  (restricted to the session Job's members when known), ignoring conhost/OpenConsole. Agent
  detection classifies image name first, then the command line normalised by
  `normalize_windows_cmdline` (so `node.exe ...\codex.js` and `cmd /c claude.cmd` resolve).
  Limits: no VM_READ means no cwd/cmdline/env (the image name still comes from
  `QueryFullProcessImageNameW` or the snapshot); parent links can outlive reused pids, so
  `ancestors` validates creation times.
- `libc` is `cfg(unix)` only; `windows-sys` and `chrono` are `cfg(windows)` deps already in Cargo.toml.
  Do not edit Cargo.toml dependencies except to add a feature to `windows-sys` (append only).

## Ownership (disjoint files; do not edit another owner's files)
| agent | owns |
|---|---|
| conpty | `sys/windows/pty.rs`, `src/process.rs`, `src/worker/spawn.rs`, `src/api/start/launch.rs` |
| job | `sys/windows/job.rs`, `signal.rs`, `src/pidfd.rs`, `src/proc_usage.rs`, `src/record/*`, `src/api/startup_containment.rs`, `src/api/startup_guard.rs`, `src/worker/{termination,procs,lifecycle}.rs`, `src/cgroup/*`, `src/placement.rs` |
| ipc | `sys/windows/ipc.rs`, `src/worker/{connection,control_socket,attach,startup,hub*}.rs`, `src/worker.rs`, `src/api/start/connect.rs`, `src/bin/aplexer/rpc*.rs`, `src/api.rs` |
| fs-paths | `sys/windows/fs.rs`, `src/paths.rs`, `src/persist.rs`, `src/history*`, `src/messaging/*`, `src/config/*`, `src/util*` |
| console | `sys/windows/console.rs`, `src/bin/aplexer/{system,terminal,attach*,input_scanner,commands,lifecycle_commands,app}.rs` |
| procinfo | `sys/windows/procinfo.rs`, `src/agent_kind*`, `src/agent_events/*`, `src/bin/aplexer/{list_tty,list_plain,status_commands,doctor,terminal_status,switching,handoff_commands}.rs`, `src/worker/runtime.rs` |
| misc-hooks | `src/hooks/*`, `src/coordination/*`, `src/task.rs`, `src/handoff_schedule.rs`, `src/awareness/*`, `src/bin/aplexer/cli_*.rs` |
| tests | `tests/*`, `src/**/tests*`, `src/bin/aplexer_tests/*`: add `#[cfg(unix)]`/Windows variants |
| ci-pkg | `.github/workflows/*`, `scripts/*`, `python*/`, `README.md`, `docs/` (not this file) |

## Rules
- Work in your own worktree/branch. Commit often on your branch; do not push or merge.
- `cargo check --locked` errors outside your files are expected; fix only yours.
- Public seam you export must be documented in a short `//!` header in your file so others can call it.
- If you need a type another owner defines, call it by the name given in the header comments of
  `sys/windows/*`; if it doesn't exist yet, add a `todo!()`-free minimal local shim and note it in your final report.
- Report: files changed, seam API exported, anything left undone.
