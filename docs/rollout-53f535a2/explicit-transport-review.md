# Explicit textual transport: recipe for ROOT review

Review gate: this recipe is prepared, **not runtime-qualified against an old
live worker**. ROOT must review this exact recipe and designate one already
existing, owned Codex recipient by full UUID before the single runtime send.
An owned recipient here means its coordinator explicitly authorizes this
control at an empty, genuinely ready prompt. Neither the active source author
nor a sibling product writer is an implicit control recipient.

## Pins and compatibility boundary

Use only `/home/alexey/tmp/aplexer-repair-0421/transport/aplexer`, SHA-256
`737de883de4f0d5f3d572517393bd027af2c38375012b8b61046c92e9e01ffe8`.
Transport source commit: `de0c6274c7932e5beb2806ef421d76810d94aef6`, tree
`1e0da2f7b67ec7adfb4c5e02f122faa2bc28a0fa`. Relevant current transport
files equal that commit byte for byte; per-file hashes are frozen in
`/home/alexey/tmp/aplexer-repair-0421/explicit-transport-review-5819481/read-only-evidence.json`.
The binary is the previously staged and tested build; this audit verifies its
exact bytes, rather than claiming a reproducible rebuild.

`src/protocol.rs` is unchanged across de0. Protocol version 1 retains the same
length-prefixed JSON request and data-frame format and `Operation::Send {bytes}`.
`rpc_send` sends each byte slice using that operation; it neither negotiates
nor starts a new worker. `read_response` validates version, request ID and
success, then accepts a JSON result without requiring the new ACK fields.
The existing transport controls use a real socket and a successful empty
result body, so legacy successful ACKs do not require the new worker fields.
This establishes source/wire compatibility; it does not infer an unknown
live worker's source revision from its pathname or certify its actual
consumption. Pin the chosen live worker's PID, recorded birth, boot, executable
device/inode and matching image mappings, normalizing only the literal
` (deleted)` suffix. Preserve unknown source provenance as unknown.

For nonempty UTF-8 text and a Codex-family engine, `--stdin --enter` sends
`ESC[200~`, the exact text, `ESC[201~`, then a separate CR after the existing
300 ms delay. The delay is framing behavior, not readiness or consumption
evidence. `--raw` and `--hex` explicitly request literal input; neither is
part of the textual control. No new wire operation, daemon, worker, global
binary swap, PATH change, hook change or registration migration is required.
Existing workers retain their existing state/freshness behavior.

Prior actual child consumption is preserved at
`/home/alexey/tmp/aplexer-repair-0421/transport-corrected-oracle/`: exact
5247-byte user text, payload SHA-256
`4965d558a3f16dfccd778632ee727a8fdb414da6d707d9d8a43038616369bc86`,
and final assistant reply `APL0421STDINPASS`, with zero tool calls.
That control used a newly staged worker and is not an old-worker qualification.
The earlier `transport/result.json` INVALID_ORACLE is excluded.

## One owned existing recipient, after recipe review

1. Record ROOT's recipe acceptance, full recipient UUID, owning coordinator's
   authorization, exact native Codex log path, engine, workspace, socket,
   worker/workload PID identities, and an empty-composer inspection. Bind the
   native log by recorded engine session identity and actual matching prior
   conversation; never accept automatic transcript fallback. Preserve a raw
   log copy and its current event/byte boundary before sending.
2. Obtain current Status and screen through the explicit client. Evaluate the
   returned live record using the unchanged source policy
   `session_ui_state` / `watch::derive_agent_state_with_source` and the same acceptance rule as
   `message_deferred::require_ready_prompt`: source `reported`, state `idle`
   or `waiting`, running lifecycle and exact live registered worker identity.
   Preserve the raw status, decision time, reported timestamps and reason.
   Require the owning coordinator's contemporaneous empty-prompt confirmation
   and no conflicting running turn/modal/composer. Screen or historical Idle
   alone is insufficient. Direct `send` checks attachability but does **not**
   itself enforce this semantic gate. On unavailable or contradictory evidence,
   leave the input unsent. Do not refresh state artificially or relax a TTL.
3. Create one immutable, nonempty UTF-8 control file exceeding 5247 bytes,
   containing multiline Unicode text and a fresh nonce. Request only the exact
   nonce-bearing reply, with no tools, edits or agents. Freeze its exact bytes,
   length and SHA-256 before sending. The recipient authorizes that entire
   payload. Recheck binary SHA, recipient binding, PID/birth/image and readiness
   immediately before the one invocation below; any change aborts it.
4. Invoke the explicit binary once, using the full UUID. Save stdout, stderr,
   exit code and start/end times without inventing a receipt on command failure.
   A transport timeout, partial write or missing reply is uncertain: no retry,
   appended CR, repeated keys or alternate submission.
5. Read only the bound native transcript after the saved boundary, preserving
   raw log and parsed events. Require a new user message equal to the whole
   payload (UTF-8 bytes/hash), then a new final assistant **message** equal to
   the nonce reply. Tool-call strings, this recipe, parent logs and captured
   screen echoes are not consumption proof. Allow bounded observation for at
   most 60 seconds; absent proof yields `consumed: null`, never a resubmit.
6. Recheck the exact protected worker identities/image mappings, target
   hardlinks, hooks and configuration pins. Persist a separate semantic
   evidence receipt, all artifact hashes and any measurement errors. Retain
   every live worker and registration; do not kill the existing recipient.

The command shapes below are a recipe, not an instruction to execute before
review. Substitute only the accepted full UUID, owned fresh receipt directory,
immutable payload and already verified native log path. Do not substitute an
arbitrary tag or use `start` to produce a recipient.

```bash
/home/alexey/tmp/aplexer-repair-0421/transport/aplexer --json status "$recipient_uuid" > "$receipt_dir/status-before.json"
/home/alexey/tmp/aplexer-repair-0421/transport/aplexer capture "$recipient_uuid" --screen --plain > "$receipt_dir/screen-before.txt"
# Only after the documented genuine-readiness and identity gate:
/home/alexey/tmp/aplexer-repair-0421/transport/aplexer --json send "$recipient_uuid" --stdin --enter < "$payload_file" > "$receipt_dir/pty-write.json" 2> "$receipt_dir/send.stderr"
/home/alexey/tmp/aplexer-repair-0421/transport/aplexer transcript "$recipient_uuid" --path "$native_log" > "$receipt_dir/transcript-after.jsonl"
```

The client receipt remains `status: pty_written`, `enter_written: true`,
`consumed: null`; `bytes` counts payload plus CR, excluding framing bytes.
A separate evidence receipt may set `consumed: true` only with the exact new
user event and final nonce reply, referencing their raw log offsets/event IDs,
payload hash, recipient binding, command receipt and preserved worker pins.
Normal coordinator textual sends may use this absolute binary only after
ROOT accepts the resulting old-worker evidence, and must keep the same
ownership/readiness gate. Durable queued-message delivery should retain its
existing guarded/idempotent delivery path; direct sends do not add idempotency.

## Immutable administrative references and current proof

Administrative source remains `58194819424986bc9cc8358293688e26537b86c6`,
tree `77954664e59f6e9e2f85544d606e4ae9634c7747`. Freeze:
`/home/alexey/tmp/aplexer-repair-0421/freeze-5819481/freeze.json`, SHA-256
`c7acb4dcb7c7a345a6b9124557051d0f2541ac940e95cc18765defd3cd15a417`.
Every listed artifact hash verifies. Its refused rev4 `7a06` receipt and
original 11 transaction controls remain preserved. The rev5 refusal and 12
registration controls remain frozen; no outcome is relabeled.

Fresh read-only exact-registration proof:
`/home/alexey/tmp/aplexer-repair-0421/registrations-1774153b29ab/report.json`.
All 35 current protected registered worker PID/birth/executable inode/mappings
equal the prior frozen snapshot. Classification remains 35 LIVE_WORKER,
1 TERMINAL_ABSENT, 3 UNKNOWN, 5 OUTSIDE_PRIOR_ACTIVE_SCOPE.

| Registration | Recorded worker | Current classification |
| --- | ---: | --- |
| 7bc9cc08-0aa5-4346-9d7b-2c9dd8fd7714 | 3986427 | UNKNOWN: no authoritative containment locator; shared scope has 50 actors |
| 6ef7413b-7bfe-4127-b58e-69d775e286e2 | 2552583 | TERMINAL_ABSENT: absent leaders and absent authoritative UUID containment in verified domain |
| 3f154b52-80ad-4770-b60c-31718208fe64 | 2382710 | UNKNOWN: inherited scope absence is not authoritative own containment |
| b5883ba1-e006-4144-acd5-58c1ba3f1251 | 997321 | UNKNOWN: inherited scope absence is not authoritative own containment |

The report preserves 288 attribution read errors; no attributable descendants
observed does not prove universal absence. All records and actors are retained.
**The six-target rollout remains refused.** Explicit client use does not open
that gate or qualify candidate `53f535a2` for installation.
