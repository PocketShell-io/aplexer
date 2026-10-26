# macOS port: done

Status (2026-10-26): **done — aplexer builds and links for
`x86_64-apple-darwin` and `aarch64-apple-darwin`, and the release script
ships both.** `src/lib.rs` gates the crate to Linux, macOS, and Windows:

```rust
#![cfg(any(target_os = "linux", target_os = "macos", windows))]
```

Cross-compile from the Hetzner Linux host with
`cargo zigbuild --target {x86_64,aarch64}-apple-darwin` (zig bundles the
macOS SDK; plain `rustc` needs a wrapper because it passes
`-exported_symbols_list`, which zig's driver rejects) or with
`scripts/build-all.sh`, which builds all six targets, verifies Mach-O
headers, and assembles the `macosx_*` wheels. Runtime proof runs on
GitHub-hosted `macos-14` (Intel) + `macos-15` (Apple Silicon) runners in
the `alexeygrigorev/aplexer-macos-ci` mirror (the PocketShell-io org
cannot run Actions): `cargo test` plus a start/list/stop smoke.

## Backend map

Unix code shared with Linux stays behind `#[cfg(unix)]` (PTY setup,
Unix-domain sockets, `flock`, `waitpid`, signals, `setsid`). Every
Linux-only API got a macOS backend; Linux paths are untouched behind
`#[cfg(target_os = "linux")]`.

| # | Area | Linux | macOS (`sys::macos`, `cgroup::macos`) |
|---|------|-------|----------------------------------------|
| 1 | containment | cgroup-v2 scope (`src/cgroup/{scope,systemd,recovery}.rs`) | process group: the workload is born into its own pgid (`setsid()` in the spawn `pre_exec`), teardown is `killpg` (`cgroup/macos.rs::Cgroup`). Created for every session in `worker/startup.rs`, persisted as a `pgid-<pgid>` locator. |
| 2 | limits | `ScopePlan` (MemoryMax/TasksMax/CPUQuota via systemd-run) | fail closed: `ScopePlan::prepare` bails when limits are requested rather than run unenforced. No launchd/rlimit story is wired. |
| 3 | mailbox wakeups | inotify (`messaging/wait.rs`) | kqueue `EVFILT_VNODE` (`NOTE_WRITE/EXTEND/DELETE/REVOKE`) on the mailbox dir; `NOTE_DELETE/REVOKE` invalidates like `IN_IGNORED`. |
| 4 | worker wakeups | eventfd (`worker/termination.rs`) | nonblocking self-pipe per channel (eventfd does not exist; `EVFILT_USER` is not async-signal-safe). Single-fd shape kept. |
| 5 | sockets | `SOCK_CLOEXEC`/`SOCK_NONBLOCK` | plain socket + `fcntl` (`api/start/connect.rs`, `rpc_connection.rs`, `system.rs` pipe). |
| 6 | reaping boundary | `prctl(PR_SET_CHILD_SUBREAPER)` | no-op: XNU has no subreaper; daemonized descendants reparent to launchd (pid 1). The libproc tree walk still finds everything that stayed. |
| 7 | process identity | pidfd + `/proc/<pid>/stat` start time | libproc `PROC_PIDTBSDINFO` start time (`sys::macos`), pinned the same way (`pidfd.rs::macos_impl::PidHandle`). |
| 8 | tree walks | `/proc` task `children` files | `proc_listchildpids` / `proc_listallpids` + ppid map. |
| 9 | liveness | `kill(pid,0)` + `Z`-with-empty-thread-group | `kill(pid,0)` + `pbi_status == SZOMB` (per-process on macOS, no thread-group second opinion needed). |
| 10 | socket creds | `SO_PEERCRED`/`ucred` | `LOCAL_PEERCRED`/`xucred` (`SOL_LOCAL`). |
| 11 | atomic publish | `renameat2(RENAME_EXCHANGE)` | `renameatx_np(RENAME_SWAP)` (APFS); non-atomic `rename` fallback on filesystems without swap support, so the startup exchange probe still passes. |
| 12 | PTY name | `ptsname_r` | `ptsname` (static buffer copied immediately). |
| 13 | signal reset | `1..=SIGRTMAX` | `1..=31` (`NSIG`; no real-time signals on macOS). |
| 14 | scope escape | systemd system scope | `system_scope_escape_decision() == Ok(false)`: no per-user-manager exit domain to escape, no launchd placement wired. |
| 15 | accounting | `/proc` stat scan | libproc `PROC_PIDTASKINFO` user+sys ns (`clock_ticks_hz == 10^9`), ppid map. |
| 16 | cwd tracking | `/proc/<pid>/cwd` | `PROC_PIDVNODEPATHINFO` (`pvi_cdir`). |
| 17 | session discovery | `/proc/<pid>/environ` walk | `KERN_PROCARGS2` walk (same `APLEXER_SESSION_ID=` lookup). |
| 18 | agent detection | `/proc` comm/cmdline/environ | libproc name + `KERN_PROCARGS2` argv/environ (`sys::macos::proc_argv_of`). |
| 19 | boot identity | `/proc/sys/kernel/random/boot_id` | `kern.boottime` (same reboot-detection role). |
| 20 | doctor | cgroup probe + placement | `pgid_containment_check` (containment ok, limits refused → warning); socket path bound 104 (`sun_path` on macOS). |

## Semantics that differ (documented, not hidden)

- **No cgroup limits.** `a start --memory/--pids/--cpu-*` fails closed
  with a message naming the Linux backend. Unlimited sessions are fully
  contained via their process group.
- **pgid escape.** A descendant that calls `setsid()`/`setpgid()` leaves
  the group without leaving the session — the same hole unlimited Linux
  sessions have. The libproc descendant-tree teardown and the reap verdict
  cover everything that stayed.
- **No subreaper.** Double-forked daemons reparent to launchd; launchd
  reaps orphaned zombies promptly, so liveness answers stay truthful.
- **No OOM accounting.** `oom_killed` is always false; `stats` reports
  `{backend: "pgid", pgid, populated}`.
- **No systemd/launchd placement.** Workers and workloads live in the
  ambient launch context behind `setsid()`; there is no manager-exit
  failure domain to escape from.
- **Agent event-log tailing** (`agent_events/locate.rs` `/proc/<pid>/fd`
  scan) has no macOS backend yet: agent *detection* works, live event
  streaming from agent log files does not.
- **pid namespaces** do not exist on macOS: `kill_esrch_proves_absence`
  is always true.

## Verification

- `cargo check --target {x86_64,aarch64}-apple-darwin --all-targets`
  clean, zero warnings (Rust 1.85.0).
- `cargo test` on Linux green (lib 572 + bin 225 + integration, serial).
- `scripts/build-all.sh` verifies Mach-O headers (`MH_MAGIC_64`,
  `CPU_TYPE_X86_64`/`CPU_TYPE_ARM64`, `MH_EXECUTE`) and wheel tags.
- Mirror CI (GitHub-hosted macOS runners): `cargo test` +
  `a --version` / `a start` / `a list` / `a kill` smoke on both arches.
