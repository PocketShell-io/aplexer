# PSC-RPC-1 (aplexer stdio helper)

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
