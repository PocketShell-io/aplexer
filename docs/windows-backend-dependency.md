# Windows backend dependency: owner audit and build contract

Source owner: existing Aplexer author `40d8eebf`; installer producer:
`3e7c5392-1648-4c0e-ad80-10e262147e66` (`pocketshell-cli`, workspace
`/home/alexey/git/pocketshell-cli`). Coordination uses durable inbox messages,
not terminal injection into a working producer. Linux transport qualification
remains accepted but unsent. Linux global binaries/workers/registrations and
the three UNKNOWN administrative registrations are outside this work.

## Finding and artifact identity

Maintained source audited: commit `222246eb5fa943377090602285a0e1553e40e2eb`,
tree `386e3c89f683fa52ac8dd67d4be6c9ec06d9942d`, version `0.1.10`.
Native Windows code exists: ConPTY, SID-bound local named-pipe IPC with
owner-only DACL, Job Object containment, process identity using creation
times, attach/capture/resize, and configurable shell launch. It is neither
an SSH server nor an SFTP server. Those capabilities require the producer's
separate SSH/SFTP closure and end-to-end integration tests.

The checked GitHub release inventory contains no Windows assets, including
v0.1.10. README's assertion that every release contains Windows EXEs is not
supported by that inventory. `release.yml` makes Windows testing/building
non-blocking for Linux and includes Windows wheels only as an optional pair.
A successful overall release is therefore not Windows delivery evidence.
Recent CI run `37913466562` has successful Windows MSRV compilation but a
failed Windows validation job; its head is `776f7932...`, not this author's
HEAD. Do not transfer its compile success or failures to this source snapshot.
Raw release/job responses are preserved in the evidence directory below.

**Recovered private lineage:** the exact EXE
`07ae4821f9e13c8d4083dd490e8b664286f033678688b7c79f8b77a5f8cfeadd`
exists as `aplexer_cli/bin/aplexer.exe` inside the private wheel
`aplexer-0.1.10-py3-none-win_amd64.whl`, SHA-256
`6280878b40ad8a40f916d6bff45c126cf36d7d2c87a3a1df8a73be29d75d7d1a`.
Private source commit `a7cca1eb9b47a0e5001bf2fb2b491e1111f81c5e`, tree
`17bec32033892db434d538c57555a53439078ca6`, is independently verified in
the preserved Git bundle review. That source plus the disclosed
`src/python.rs` patch produced the recorded artifacts, with Rust 1.95.0,
Maturin 1.15.0 and Windows MSVC build receipts. It is not this current HEAD.

Preserved review:
`/home/alexey/tmp/pocketshell-windows-acl-review-20261008/aplexer-private-review.md`.
Bundle root: adjacent `aplexer-isolated/bundle`. This audit independently
rechecked all 54 manifest entries, both wheels' complete RECORD coverage,
sizes and hashes, and exact embedded payloads without executing them.
Source-to-machine-code derivation relies on recorded native build receipts;
neither reproducible rebuilding nor signed release provenance is claimed.
Do not claim the currently installed host copy equals these bytes without
an actual hash measurement of that copy.

Client wheel SHA-256:
`e4c594bce48c10686031c95d5a27536fb5636f7b8574d44f0398d6cc0bc465fd`;
its `_native.pyd` SHA-256:
`f56f717aa37fd3eafb842685f362b179069aba57b994aa8c7d3a9d24a95f4992`.
The recorded basic Python-client lifecycle smoke is not a CLI-driven
PowerShell attach, SSH terminal or SFTP acceptance test. No Rust unit-test
execution receipt is supplied by that private build.

## Producer invocation, configuration and DLL seam

Use a catalogued, hash-verified absolute `aplexer.exe` pathname for subprocess
calls. It is both CLI and worker: the native process recognizes its own
`aplexer.exe`/`a.exe` basename and re-execs itself as `worker --id UUID`.
Do not rename it to an unrecognized basename or allow an incoming
`APLEXER_WORKER` override to replace worker authority. JSON control calls use
`<absolute-exe> --json <command> ...`; interactive attach uses the native
byte stream, never JSON encoding of terminal output. Use argv arrays and
Windows-aware quoting at the SSH command boundary, including spaces and
Unicode paths. Never rely on POSIX shell expansion in PowerShell.

Set explicit absolute `APLEXER_CONFIG`, `APLEXER_STATE_DIR` and
`APLEXER_RUNTIME_DIR` consistently in every control/attach/worker context,
with private ordinary-user-owned directories. Windows defaults are
`%APPDATA%\aplexer\config.toml` and `%LOCALAPPDATA%\aplexer\{state,run}`.
Use the same non-elevated user/SID across SSH, tray and workers; named pipes
are local and SID-bound. Linux `/proc`/cgroup proof does not apply to Windows.

PowerShell is a maintained shell option. Pin an explicit array in the
installer-generated Aplexer config, e.g. on an ordinary x64 host:

```toml
shell = ['C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe', '-NoLogo']
```

Render the actual supported Windows system path from the producer's host
contract; this is an example, not permission to assume every host uses `C:`.
Invoke `start --engine shell --workspace <absolute-workspace> --tag <tag>`.
Do not change configured model/provider/profile/default engine merely to
select the terminal shell. Explicit `APLEXER_SHELL` wins over
`[engines.shell]`, which wins over this `shell` key; ensure no conflicting
engine table is generated and no incoming override changes the closed
selection. With no explicit setting the source prefers Git Bash, then pwsh,
PowerShell, cmd; a generic installer must not depend on that search order.
No execution-policy override is needed or authorized by this audit. A shell
may enforce its normal policy; agent `.ps1` shims must not silently bypass it.

Recorded PE imports distinguish three separate dependencies:

| Component | Actual dependency/ABI | Producer obligation |
| --- | --- | --- |
| Native private EXE | AMD64 PE32+, Windows system APIs/UCRT and `VCRUNTIME140.dll`; no MSYS import | Measure actual built PE imports and provide a reviewed runtime closure or verified prerequisite; do not assume VC runtime is installed |
| Optional Python client | CPython 3.11+ stable ABI, AMD64 `_native.pyd`, `python3.dll`, `VCRUNTIME140.dll`; exports `PyInit__native` | Ship matching client/CLI pair and complete interpreter runtime if this path is used |
| Optional Git Bash | Separate `bash.exe`/MSYS distribution | Required only if selected shell or SSH adapter actually relies on it; pin full distribution lineage |

`aplexer.dll` is not a documented C ABI for PocketShell; Cargo's `cdylib`
declaration does not establish one. PocketShell's examined adapter uses the
CLI JSON interface. The private Python CLI wrapper also has a reproduced
same-size cached-EXE authority gap. Explicit payload execution avoids that
cache; if using the wrapper, preserve `APLEXER_RUN_IN_PLACE=1` and hash-check
the actual distribution payload, rather than treating the console launcher
filename as pinned native bytes.

The examined guardian policy requires a pinned `backendDLL` alongside
`backendExecutable`, `backendConfig` and SFTP roles. Its previous MSYS value
cannot be silently removed or relabeled as `_native.pyd`/`VCRUNTIME140.dll`.
Producer must resolve that schema meaning with its guardian owner, bind the
real runtime imports, and retain all capabilities. PowerShell selection
alone does not eliminate a POSIX shell used elsewhere by its SSH dispatcher.

## Source-bound normal build and test path

Use a fresh isolated Windows x64 build directory, not a target installation.
Export this exact source commit/tree with an archive hash. Record rustc/cargo,
Windows SDK/MSVC `cl`/`link`, OS architecture/version, Python and build-tool
versions; preserve stdout/stderr/exit for each command. No source update or
unrecorded patch during the build is accepted.

The repository's native release recipe is:

```powershell
cargo +1.85.0 build --locked --release --bins --target x86_64-pc-windows-msvc
& .\target\x86_64-pc-windows-msvc\release\aplexer.exe --version
```

Run the existing release test lane first, with Rust 1.85.0 active:
`scripts/check-test-execution.ps1 -SelfTest`, then
`scripts/check-test-execution.ps1 -Min 380 -- cargo test --locked --release --verbose`.
Preserve counts, failures and skips. Relevant existing native suites include
`windows_attach_tty` (resize, Unicode/paste, detach/workload exit),
`windows_handle_inheritance`, `windows_job_limits`, `windows_session_discovery`,
`windows_shims`, and shell selection tests. The Git Bash-specific default
shell test may skip when Git is absent; that is not a PowerShell acceptance
result. CI's `validate.ps1` adds formatting/clippy/Python suites with Rust
1.95.0; preserve its actual result instead of assuming a workflow passes.

For wheel assembly, `scripts/build-wheels.py --platform windows-amd64`
consumes staged `aplexer.exe`. CLI metadata requires
`aplexer-client==0.1.10`; a CLI wheel is not a standalone installer closure.
The maintained Python build currently references Unix-only `libc::SIGHUP`
under feature `python`, so the normal Windows two-wheel build has a known
source blocker. The private compile patch conditionally imports platform
SIGHUP and retains unsupported-HUP behavior; it is **not applied here**.
Any normal maintained binding build must first integrate/review that fix,
then build with pinned Maturin and run cold-install plus native tests.
Native `--bins` with default features does not enable this Python path.

Before installer acceptance, producer must demonstrate under its ordinary
user SSH/tray endpoint: session list/create, PowerShell marker consumption,
real SSH attach/input/resize/Unicode/paste, disconnect/reattach persistence,
scoped kill including descendants, correct private state/config and worker
image, and SFTP upload/download/rename/list with exact byte roundtrip.
Preserve missing-capability errors and all native receipts. A source compile,
filename, `--version`, Python smoke, or shell-only endpoint is insufficient.
No Windows build or end-to-end control was executed in this Linux audit.

## Review artifacts and unresolved boundary

Evidence directory:
`/home/alexey/tmp/aplexer-repair-0421/windows-backend-review/` contains the
current source hashes, newly verified private wheel lineage, GitHub release
inventory/job snapshots, and immutable freeze manifest.
Inbox request `01a123ff-6925-77e3-a383-c9480fd3cea5` asks producer for its
exact source, invocation/env/config/DLL role and capability contract;
`01a123ff-e039-7570-a0f0-9386620c97b0` supplies recovered private provenance.
Until that interface is bound, normal runtime closure and PowerShell-only
endpoint integration remain unqualified. There is a concrete maintained
native build path and a verified private artifact, but no reviewed normal
Windows artifact or complete generic installer acceptance inferred here.
