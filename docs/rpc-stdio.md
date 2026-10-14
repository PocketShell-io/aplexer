# PSC-RPC (aplexer stdio helper)

`a rpc --stdio --protocol N` (N = 1 or 2) is the versioned resident helper.
Protocol 2 is a strict superset of protocol 1: every v1 method behaves
identically under `--protocol 2`, and `--protocol 1` output is byte-for-byte
unchanged. A protocol-1 client never receives event frames. The sections
below describe protocol 1; protocol 2 additions are in "PSC-RPC-2" at the end.

Launch `a rpc --stdio --protocol 1`. Stdout contains only frames; diagnostics go to stderr. Frames are unsigned uint32 big-endian byte length followed by UTF-8 JSON, nonzero and <=16 MiB. Truncated or oversized frames terminate the channel without executing them. Requests: `{protocol:1,id:"unique string",method:"ping",params:{}}`. IDs must be unique within a helper lifetime. Responses: `{protocol:1,id,result:...}` or `{protocol:1,id,error:{code,message,notDispatched}}`. Responses may arrive out of order. At most 32 requests execute concurrently; bounded admission applies backpressure. Duplicate IDs are rejected and never executed again. No automatic send replay after dispatch or channel loss.

Methods and params:

- `capabilities {}` -> `{protocol:1,version,maxFrameBytes,maxSendBytes,maxCaptureBytes,methods:[...],generation}`.
- `ping {}` -> `{ok:true,generation}` (live helper round trip).
- `list {running?:false}` -> existing `a list --json` array. Response sibling `meta:{generation,ageMs}` identifies cached snapshot freshness. Background registry updates and reconciliation refresh derived agent/liveness/warning/process data. Index failures are explicit errors, never silent stale success.
- `status {selector:string}` -> existing `a status --json` object; worker status is queried live, cached background enrichment is identified by response `meta`.
- `capture {selector:string,mode?:"history"|"screen"|"screenPlain",maxBytes?:16384}` -> existing `a capture --json` shape `{id,bytes,encoding:"base64",data,utf8?}`. Maximum capture 1 MiB; screen captures exceeding maxBytes fail rather than corrupt terminal snapshots.
- `send {selector:string,payload:string,submit?:false}` -> `{id,bytes}`. UTF-8 payload <=1 MiB; submitted text rejects terminal controls except CR/LF/TAB. `bytes` counts original payload plus submitted CR. Success follows final worker PTY-write acknowledgement. Per-session sends execute in arrival order; different sessions and reads can execute concurrently.

Selectors use the existing aplexer resolver verbatim: full UUID, >=8-character UUID prefix, or workspace:tag (including canonical workspace fallback). Duplicate matches prefer a sole live holder; otherwise `selector is ambiguous; use a longer UUID`. Bare tags are not accepted by the existing host CLI resolver. Selection occurs against one resident registry generation; selected UUID is reread and validated immediately before worker use, never rebound to a replacement tag. Client-specific tag selection must map to an unambiguous workspace:tag or UUID.

Error codes: `unsupported_protocol`, `invalid_request`, `unsupported_method`, `duplicate_id`, `not_found`, `ambiguous`, `registry_unavailable`, `operation_failed`, `response_too_large`. Original selector error messages are preserved. `notDispatched:true` guarantees no mutation PTY bytes were attempted; after send worker dispatch it is false even if no response arrives. Unknown methods/versions never execute. Concurrent retries with duplicate IDs cannot execute; clients must retain uniqueness and never retry uncertain sends across helper generations.

Submitted-send investigation: current worker acknowledgement proves PTY writes, not Codex/Claude event-loop parsing. Neither PTY writable readiness nor bracketed-paste mode proves application consumption. Keep existing 300 ms between text/paste and Enter; submitted sends remain outside the <=4 ms helper / <100 ms end-to-end target. No unverified delay removal.

Development binary: `/home/alexey/git/wt-aplexer-rpc/target/release/aplexer`. Installed `a` is not replaced.

## Agreed PSC compatibility amendments

`list {format:"psc"}` returns `{schema:3,sessions:[schema3 raw snake_case rows],errors:[]}`, filtered to aplexer state starting/running/exiting. Schema3 follows `pocketshell-cli/src/pocketshell/runtime/sessions.py` including basename:tag names, shell engine null, activity/report state thresholds (3000/8000ms), epoch seconds and attached/alive flags. Default list remains the aplexer array.

`status/capture/send {selectorMode:"psc",selector,...}` apply the exact `hostClient.ts::pickSession` precedence: first exact display name (workspace basename:tag) or case-sensitive UUID; union of exact tag and case-sensitive UUID prefixes >=4 characters, deduplicated by UUID. Multiple matches return `SESSION_AMBIGUOUS` plus `error.details.candidates:[{name,id}]`; zero return `SESSION_NOT_FOUND`, with PSC wording. All resolution remains host-side, within one RPC. Successful results include `session:<schema3 row>` beside original fields. `capture` also supplies `text` for valid UTF-8; base64 is authoritative for arbitrary bytes. `send` accepts `text` as alias for `payload`. Client must explicitly request format/selectorMode; defaults preserve the existing aplexer CLI contract.

## Implementation and freshness

The helper incrementally checks registry directory/record metadata every 20 ms, reparses changed records, and reconciles with the strict existing registry reader every second. This portable polling implementation uses no filesystem watcher; missed or metadata-preserving changes are repaired by reconciliation. Corruption fails closed with `registry_unavailable`; no cached list is returned after a known index failure or after two seconds without refresh. Derived agent/liveness/warning data refreshes on change and at least every 50 ms when the machine permits. All derivation uses the same aplexer APIs as the CLI. `meta.ageMs` reports enrichment age (including send queue/writes); this is a measured cache age, not a hard 100 ms freshness promise on an overloaded host. Process counts/CPU sample independently every ~500 ms plus scan cost; `meta.processAgeMs` identifies their separate age.

A list result is pre-serialized outside the request path. Worker status/capture/send still query the selected worker live with the existing bounded control deadline. Selection pins one UUID; the strict record reader checks identity/socket/history immediately before dispatch. Per-session ticket ordering serializes all sends from this channel, including bracketed-paste fragments and Enter. Other helper processes and standalone CLI writers are outside this channel's ordering domain. At most 32 worker requests run concurrently. Each helper accepts up to 65,536 distinct IDs before requiring reconnection, bounding duplicate-ID memory. Duplicate IDs return `duplicate_id` instead of replaying the first result; clients must never resend a mutation under another ID to bypass this guard.

Capabilities additionally expose `pscSchema:3`, `selectorModes:["aplexer","psc"]`, and `versions:{aplexer,pocketshell}`. The PocketShell version is probed once at cold startup with a two-second deadline and bounded output, and is null on absence/failure. `versions.aplexer` identifies this helper binary, even when the installed `a` differs. Retire/reconnect the helper to renegotiate versions after an installation change.

For `selectorMode:"psc"`, multiline send payloads use separate bracketed-paste start/body/end writes, matching `encodeSendPayload` on the client; submitted multiline input then waits 300 ms before Enter. Capture clients must map their screen mode to `screenPlain` and raw mode to `history`. The default capture is 16 KiB. Binary capture consumers decode `data`; `text` is null when UTF-8 is invalid.

Run the isolated benchmark with `python3 scripts/bench-stdio-rpc.py --binary target/release/aplexer --sessions 100 --runs 1000`, adding `--live` for 100 owned live workers. The default fixture contains one live worker and retained terminal records; this measures indexing/list size without claiming those rows are all live. Every fixture lives under private temporary registry/runtime roots and is killed through the locally built binary. The installed binary and live user registry are untouched. This benchmark includes pipe transport and Python JSON decoding in the request latency.

## Measurements (Linux, local stdio)

With 100 owned live `/bin/cat` sessions, 1,000 warm samples per method, 16 KiB captures and one-byte no-enter sends, measured request p95/p99 (ms) were:

| Method | p95 | p99 |
| --- | ---: | ---: |
| ping | 0.134 | 0.726 |
| raw list | 1.450 | 2.494 |
| PSC schema3 list | 0.971 | 2.660 |
| live status | 1.544 | 3.355 |
| capture 16 KiB | 2.420 | 4.726 |
| no-enter send | 2.730 | 5.465 |

Cold helper ready took 363.6 ms. One hundred live rename observations (including the local rename process) had p95 74.4 ms, p99 85.2 ms and max 103.9 ms. This meets the local <=4 ms p95 / <=8 ms p99 helper target for the tested load. It does not establish the gateway end-to-end target.

A separate run concurrent with repository validation failed those limits: status/capture/no-enter send p95 10.5/11.5/6.4 ms, visibility p95 389.5 ms. The client worker measured gateway operation p95 around 99–105 ms in an earlier quieter batch, and 131–199 ms under concurrent benchmark/validation load; submitted send was 436–545 ms. Overall fresh-process <100 ms remains unmet. Keep the feature opt-in, retain the correctness delay, and measure actual deployment load instead of treating a local passing sample as a universal bound.

At 500 registry rows (one live worker plus 499 terminal records), the same final binary and 1,000 samples with 16 KiB captures measured p95 raw list 7.716 ms, status 1.609 ms, capture 1.530 ms and no-enter send 2.229 ms. Registry visibility p95 was 73.0 ms (p99 110.6 ms). The raw list includes all 500 complete records and Python decoding; PSC listing excludes terminal records. This fixture therefore measures full-registry scale, not 500 live sessions.

The submission investigation follows `worker/connection.rs::handle_send`: successful `runtime.send` produces `status:"pty_written", consumed:null`. PTY writable/drained events prove kernel transport progress; they do not acknowledge the agent's key-event/paste parser. Bracketed-paste mode only advertises a terminal capability. State hooks and screen changes are not correlated acknowledgements for an individual send request. None of these safely replaces the existing text-to-Enter barrier for Codex/Claude, so this change retains it and makes no new consumption claim.

## PSC-RPC-2 (push subscriptions)

Launch `a rpc --stdio --protocol 2`. Framing, IDs, admission, duplicate-ID
rejection and every v1 method are unchanged; response and error envelopes
carry `protocol:2`, and `capabilities` reports `protocol:2` with `methods`
extended by `subscribe` and `unsubscribe`. A v1 request (`protocol:1`) to a
v2 helper is rejected as `unsupported_protocol`, and `subscribe`/`unsubscribe`
to a v1 helper fail as `unsupported_method` without dispatching.

- `subscribe {topics:[...], format?:"psc", selectorMode?:"psc"}` →
  `{subscription:"sub-N", seq}`. Topics: `sessions`, and
  `screen:<selector>` with the exact `capture` selector rules (UUID, UUID
  prefix, `workspace:tag`, plus `selectorMode:"psc"`). Unknown, empty,
  duplicated or (for screens) empty-selector topics are `invalid_request`;
  at most 16 screen topics per helper. `seq` is the first event's `seq`
  (or the next sequence number when no initial frame is due, e.g. a screen
  topic whose first capture is still failing).
- `unsubscribe {subscription}` → `{unsubscribed:"sub-N"}`; an unknown
  subscription is `invalid_request`. Unsubscribe never touches the registry,
  so it works even while the snapshot is unavailable.
- Right after the response, one `{event:"snapshot", topic:"sessions", ...}`
  carrying the same data as `list` (`format:"psc"` selects the schema-3
  listing, otherwise the raw array). Afterwards a new `snapshot` whenever the
  list result changes, coalesced to at most one per 50 ms. Change detection
  ignores wall-clock and sampler fields (`activity_epoch`, agent activity
  state, CPU/process counts, last-activity/access/update timestamps), so
  flowing output alone never spams snapshots: activity-only changes ride the
  5 s heartbeat cadence (a snapshot goes out in place of the heartbeat)
  while membership, phase, state, tags, agent, attachment and warnings still
  push within 200 ms. Activity-derived fields in the replica may therefore
  be up to 5 s old.
- `screen:<selector>` resolves once and pins the UUID. The helper pushes
  `{event:"screen", topic, data}` (the `capture screenPlain` result plus the
  `session` row in `selectorMode:"psc"`) when the rendered screen changes, at
  most 10 Hz. Screens are polled at most every 100 ms, and a topic whose
  capture fails backs off for 2 s so a wedged worker cannot stall the
  publisher tick. When the pinned session ends (record gone or terminal
  phase), it pushes `{event:"ended", topic}` and drops the topic. Per-screen
  frames are bounded by `maxCaptureBytes`.
- Event frames use the same framing, without an `id`:
  `{protocol:2, event, subscription, topic, seq, data?}` (heartbeats also
  carry `generation`). `seq` is strictly increasing per helper generation
  across all topics; events never carry `id`, so clients tell them apart
  from responses.
- Backpressure is latest-wins per (subscription, topic): a newer frame for a
  key with a queued frame replaces it in place and carries `"gap":true`; a
  heartbeat newer than queued state is dropped instead (the queued frame
  proves freshness better); `ended` evicts its topic's queued frame, carries
  the gap when it displaces one, and is itself never dropped. Overflow evicts
  the oldest non-`ended` frame and marks its key, so the gap always lands on
  the right topic; a hard 16-frame cap applies regardless. Every snapshot
  and screen frame is full state, so the client resumes from the latest
  delivered frame after a gap.
- Heartbeat: a topic with no frame for 5 s emits
  `{event:"heartbeat", seq, generation}`. The replica age is the time since
  the last frame; no heartbeat while the registry snapshot is unavailable
  (a heartbeat would certify freshness the helper cannot vouch for).
- New error codes: none; `invalid_request` covers bad topics, too many
  screens and unknown subscriptions. Selector failures reuse `not_found` /
  `ambiguous` (and the `SESSION_*` shapes in `selectorMode:"psc"`).

Implementation: `subscribe` is prepared lock-free (selector resolution and
initial screen captures first, which may block on a worker), then inserted,
answered and primed under the stdout lock, so the response always precedes
the first event on the wire and captures never stall the event writer or
other responses. A publisher thread evaluates subscriptions every 20 ms
(screen polls at most every 100 ms, 2 s backoff after a failed capture)
without holding locks across worker I/O, and a writer thread drains the
bounded queue. `send` ordering, duplicate-ID rejection, the 65,536-ID cap
and the never-replay rule are unchanged.

Measured locally (Linux, debug build, one live session): record-write to
`sessions` snapshot delivery p50 61 ms, p95 71 ms (n=20), dominated by the
50 ms coalescing window plus the 20 ms registry poll, as designed.
