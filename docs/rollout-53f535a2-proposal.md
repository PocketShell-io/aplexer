# Six-target administrative rollout proposal — rev 4, review freeze

The original wrapper at `d5c8119` remains refused and preserved in history.
This revision corrects its transaction defects. **No installed target has
been replaced. Apply requires concrete ROOT and peer source/preflight review.**

The rollout candidate remains the separately qualified, unchanged ELF
`/home/alexey/.aplexer-fix-gated-idle/bin/aplexer`, source `6dabc1ec`, SHA256
`53f535a2512f3572e61b4a0058e2506ddaa8a25186cb89b5faf17afc3aceba02`.
Its accepted genuine Stop, actual Idle, guarded delivery, recipient ACK
reply, and idempotency evidence remain the qualification basis. The wrapper
never executes that candidate during preflight and never invokes the product
state path. Text transport changes described below are a separate source
freeze and do not change this rollout candidate or qualify a new rollout.

## A. The six targets (preflight-verified 2026-10-10)

| id | full path | sha256 (current) | uid/gid | mode | inode | nlink |
|----|-----------|------------------|---------|------|-------|-------|
| T1 | /home/alexey/.local/bin/aplexer | 5655521e4881ad8e9d283434027361c0905f2f04e11a55c26ed7c473a3f1ad48 | 1000/1000 | 775 | 6314299 | 1 |
| T2 | /home/alexey/.local/share/uv/tools/pocketshell/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 790dfc6446a709499cbdf02a36d04eb3224eb352e55cbefd7937888e7cb69eb1 | 1000/1000 | 775 | 16254705 | 1 |
| T3a | /home/alexey/git/pocketshell-cli/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 0a4e6893e22429867c8d396711ecb526ad4e506627f3889dd7f6ee8969ff25ac | 1000/1000 | 775 | 11944740 | 1 |
| T3b | /home/alexey/git/pocketshell-cli-presence/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 0a4e6893e22429867c8d396711ecb526ad4e506627f3889dd7f6ee8969ff25ac | 1000/1000 | 775 | 14332318 | 5 |
| T3c | /home/alexey/git/pocketshell-cli-gateway-service/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 0a4e6893e22429867c8d396711ecb526ad4e506627f3889dd7f6ee8969ff25ac | 1000/1000 | 775 | 14332318 | 5 |
| T3d | /home/alexey/git/pocketshell-cli-windows-gateway/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 0a4e6893e22429867c8d396711ecb526ad4e506627f3889dd7f6ee8969ff25ac | 1000/1000 | 775 | 14332318 | 5 |

Out of scope, preserved untouched: every Python shim (`a`/`aplexer` console
scripts, e.g. pocketshell-cli shim 732999d4…), all hook configs, the dev venv
`~/git/aplexer/python-cli/.venv`, and the running daemon processes.

## B. Transaction and the nine corrections

1. Preflight reads metadata/full hashes, registered worker identities, hook
   hashes, and bounded hardlink inventory. No candidate execution or synthetic
   state probe occurs.
2. Exclusive fresh backups and candidate files use `O_EXCL|O_NOFOLLOW` and
   owned directory descriptors. `fchmod` enforces backup 0775 and candidate
   0755 regardless of umask; descriptor ownership/type/mode are verified.
3. Copy loops over short writes and rejects zero progress. It hashes actual
   destination descriptor bytes, checks size and `fstat`, and independently
   verifies the path still names that inode. All descriptors close on errors.
4. Backup and staging files and their directory entries are fsynced before
   a durable intended record publishes backup custody and exact staged
   dev/inode/type/UID/GID/mode/fullhash. This record precedes target rename.
   The committed outcome is published immediately after rename, before
   post-verification. A crash in either gap retains the intended identity.
5. Immediately after copying and journal publication, target and parent
   type/dev/inode/UID/GID/mode and target fullhash are rechecked. Target link
   count accounts only for prior replacements of the same original inode by
   this run. Parent must remain a nonsymlink owned directory; rename uses a
   verified held directory descriptor. No old target inode is opened writable.
6. Rollback requires the exact installed dev/inode/type/UID/GID/mode/size and
   candidate fullhash. Backups must remain verified nonsymlink regular files
   with their exact recorded identity, 0775, owner/group and original hash.
   A concurrent same-hash replacement is skipped, preserving its new inode.
7. Each rollback failure is recorded separately; reconciliation continues for
   all other entries. Restored identity is verified before reporting success.
8. Post-verification and final success journal publication run inside the
   transaction handler; failures take the same scoped rollback path. Pending
   intended records are recoverable with `rollback --manifest --force`.
9. Every JSON receipt/journal uses exclusive fresh owned temporary bytes,
   descriptor verification, file fsync, atomic same-directory rename and
   directory fsync. Failed writes preserve the preceding published journal.

Exactly the six entries in the table are replaceable. Each is a fresh copied
inode, never a hardlink or in-place write. The three names of inode 14332318
lose only their own directory entries; other hardlinks retain inode/bytes.
Bounded enumeration reports its limits; all discovered unnamed siblings are
rechecked. Undiscovered siblings remain protected by fresh-inode replacement.
Backups are retained in each target directory, receipts in
`/home/alexey/.aplexer-rollout-53f535a2/`.

Immediate rechecks and held directory descriptors reduce races; ordinary
rename is not an atomic compare-and-swap against a concurrent file editor.
Review/coordination must keep these six entries free from other writers during
an authorized apply. A postcheck discrepancy never authorizes overwriting a
changed concurrent inode during rollback.

## C. Protected workers and compatibility

The wrapper enumerates registered running workers with durable native identity
records and verifies PID, kernel birth and mapped executable dev/inode. The
postcheck follows that exact protected set rather than a global transient
process census. Executable readlink text normalizes the natural ` (deleted)`
suffix; held image dev/inode and map device/inode remain decisive. An unreadable
or absent protected worker fails closed, without asserting a restart.

Old live workers retain their old freshness behavior. The qualified candidate's
additive report fields remain compatible with them. Worker launch resolves
`APLEXER_WORKER`, current executable, sibling, then PATH fallback; this proposal
makes no claim about which code future workers execute. Worker freshness fences
require a separately authorized restart, outside this task. A genuine Stop can
produce idle, working, or no-evidence; no unconditional idle recovery is claimed.

Hook configs are only hashed. Existing generated `gated-idle` spelling remains
unchanged and parses through the qualified alias. No hook reinitialization,
shared daemon restart, shim/settings edits, seventh ELF replacement, publication,
model/provider/profile/auth changes, or global apply is included.

## D. Actual verification receipts

Transaction controls: `python3 docs/rollout-53f535a2/test_apply_rollout.py -v`.
They mutate only temporary fixtures and exercise short/zero/corrupt writes,
umask, six fresh replacements plus untouched hardlinks, durable pre-rename
intent, rename/postcheck failures, concurrent identity changes, invalid backups,
partial rollback, atomic journal failure, and deleted-suffix normalization.

Metadata-only preflight:
`/home/alexey/.aplexer-rollout-53f535a2/preflight-6fef0ecebbd2/receipt.json`.
All six original pins and the candidate fullhash verified; 35 registered live
worker identities were read. Result **FAIL** because four registered identities
reference absent PIDs: 3986427, 2552583, 2382710, 997321. Those failures are preserved,
not erased or synthesized away. No targets changed. ROOT review must consider
this actual refusal before authorizing any apply.

## E. Separate textual stdin transport source repair (de0c627)

`send --stdin --enter` previously selected Raw regardless of content, disabling
Codex's explicit bracketed paste. Preserved exit-0 raw-stdin and raw-CR receipts
0386/0394 plus pending-composer screens 0388/0396 demonstrate that PTY ACK did not
mean user-turn consumption. The supported positional-text recovery 0417 used
nonempty Codex text framing and consumed the pending request.

The corrected source selects Text for UTF-8 stdin with Enter, just as for
positional text. `--raw` or `--hex` opts into literal bytes; binary/terminal-control
input with textual Enter is refused before writing. Without Enter, stdin bytes
remain literal. Empty Enter stays a single carriage return. The existing
nonempty, engine-aware bracketed paste and separate Enter path remains intact.
Guarded message readiness, TTLs, idempotency and refusal conditions are unchanged.

Successful transport reports `status=pty_written`, `enter_written`, and
`consumed=null`; neither flush nor elapsed delay is semantic submission evidence.
Consumption is established separately by native user-message content and a
recipient response. RPC ACK similarly describes PTY write only.

The real Codex transport control used existing configured defaults, no model or
auth changes, and the supported run-without-daemon choice. Its exact 5,247-byte
multiline Unicode stdin became a native user message followed by the final
assistant reply `APL0421STDINPASS`, with no tool calls. Full receipts, exact payload,
native log and history are preserved under
`/home/alexey/tmp/aplexer-repair-0421/transport-corrected-oracle/`. Supported kill
removed exactly its owned worker/workload; absence was independently checked.
The earlier control under `transport/` has `INVALID_ORACLE` recorded: initial
fallback selected the parent transcript and matched a tool-call mention. It is
preserved and excluded from passing evidence.

Validation: 519 library and 214 binary tests passed; 4 transport regressions and
14 guarded delivery controls passed. One pre-existing unused-mut warning in
`doctor.rs` remains. This source freeze is reviewable and has not been installed.
