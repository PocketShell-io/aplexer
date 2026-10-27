# Exact-session retirement evidence

Native consumers may read `$APLEXER_STATE_DIR/retired-sessions/<full UUID>/tombstone.json`
(on Unix the default root is `$XDG_STATE_HOME/aplexer` or
`~/.local/state/aplexer`; on Windows it is `%LOCALAPPDATA%/aplexer/state`). This is the public durable retirement
receipt; Rust consumers can use `retired::read_finished_tombstone(paths, uuid)`.
Do not resolve a tag to its latest incarnation for an exit diagnostic. Validate
`id`, `workspace`, `tag`, and `created_at_ms` against the expected launch. The
UUID plus creation timestamp identifies the incarnation. No host census or
capture request before exit is required. Normal natural retirement remains
enabled; this contract does not require `keep_exited`.

Enable bounded retirement output on each owned launch using
`a start --env APLEXER_RETAIN_RETIRED_OUTPUT=1 ...` (or the equivalent explicit
`StartOptions.env` entry). Only the session record's explicit env map enables
this behavior; an ambient worker environment variable does not. Sessions without
this opt-in keep their existing metadata-only tombstones. No global setting or
retrospective control of other sessions is needed.

Opted-in worker receipts atomically publish these optional additions before deleting
the active state directory:

* `created_at_ms`: original native launch time (wall duration is
  `exit.exited_at_ms - created_at_ms`, in milliseconds).
* `containment_empty: true`: worker observed the entire containment domain empty
  before publishing this receipt. This is independent of the leader's status.
* `output`: `{status, observed_bytes, limit_bytes, bytes?}`. `bytes` is a JSON
  array of raw PTY octets, including terminal escapes and potentially invalid
  UTF-8. stdout and stderr share the PTY; original pipe boundaries do not exist.

`output.status == "complete"` means `bytes` contains the complete observed PTY
stream, including a valid empty stream. Its length equals `observed_bytes` and
is at most `limit_bytes` (currently 65536). No final clipping or text decoding
has occurred. Consumers must decode safely and sanitize the **entire** evidence
before final display clipping, including secrets split across display boundaries.
Raw evidence is sensitive and must not be logged before sanitization.

`"oversize"` has an observed byte count and no `bytes`: either the stream exceeds
65536 bytes or the configured history ring discarded part of it. A tail is never
represented as complete evidence. `"missing"` has no `bytes` and an unknown byte
count: the worker could not read its output buffer. Never describe either status
as captured output. Missing receipt/output/identity/containment fields in older
receipts are unknown, not zero/empty/success. Existing `exit` code, signal, OOM,
and exit wall timestamp semantics are unchanged.

The receipt is owner-private and follows the existing 30-day tombstone pruning
policy. A failed receipt publication preserves active state and keeps the worker alive
with capped-backoff retries, retaining the in-memory evidence until publication
succeeds or an explicit external retirement removes the active record. Supersede archives are a separate
contract. Other removal paths may carry exit status without output; consumers
must handle that explicitly. No consumer needs to control or delete any session
in order to read this exact UUID receipt.
