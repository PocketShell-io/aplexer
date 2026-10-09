# Qualification recipe — gated Stop candidate `f6ac6e7` (rev 3)

**Status: FROZEN rev 3 for root review — NOT EXECUTED.** Applies review
01a122f8: fully executable (no human permission round), hooks via
`--setting-sources project,local` + owned `settings.local.json`, fake-HOME
init dropped, doc-HEAD vs binary-source pins separated, `send --stdin --enter`
fixed bytes, same-ID re-delivery refusal oracle. Supersedes rev 2 (1f53998).

## Pins (distinguished)

- **Binary source pin** `f6ac6e78a2fe3b5d7a0b42255c84d1533a1dfe45`; staged
  binary `/home/alexey/.aplexer-qual-f6ac6e7/bin/aplexer` sha256
  `0ee40cea79c72a2c5615ed82a9557de763c053033fb88dacc92714b451edc2f0`
  (reports `a 0.1.10`; `$QUAL/bin/a` symlink).
- **Doc HEAD at freeze:** this commit (docs-only on top of `f6ac6e7`).
- Preflight asserts BOTH: staged sha equals the binding;
  `git merge-base --is-ancestor f6ac6e7 HEAD` succeeds; and
  `git diff --quiet f6ac6e7 HEAD -- src Cargo.toml Cargo.lock` proves the
  docs-after-f6 touched no code.

## Verified surface (this host, recorded)

- `claude --version` → `2.1.289 (Claude Code)`; `claude --help` lines 225/227:
  `--setting-sources <sources>`, `--settings <file-or-json>`.
- `a start [OPTIONS] [-- <COMMAND>...]`: an explicit `--` command replaces the
  engine default; with a command present, `resolve_launch`
  (`src/api/start/claim.rs:22,31`) does NOT append skip-permissions argv;
  `--engine claude` records `engine=claude` on the record
  (`Config::resolve` preserves the explicit engine with direct argv).
- `a send --stdin --enter` (`a send --help`): fixed bytes from stdin.
- `a init --check --json`: no-touch contract (used read-only, no init run).
- Delivery gate `src/bin/aplexer/message_deferred.rs:56`: proceeds iff
  `source == "reported"` and state ∈ {`waiting`, `idle`}; reported `running`
  refuses (":75 recipient reported working"). Sender/recipient authorization:
  `message_deferred.rs:13` + `process.rs:87` ancestor-environ walk.
- Hooks: `CLAUDE_EVENTS` (`src/hooks/mod.rs:152`); command builder
  `state_report_command` (`:214`) = `'<a_bin>' state-report <state> || true`;
  nesting shape per module docs `:50`. `a start`/worker install NO hooks
  (no hooks reference under `src/api/start/`).

## Owned hook config — `$QUAL/work/.claude/settings.local.json`

Derived from exact f6 `CLAUDE_EVENTS`/`state_report_command`; absolute staged
binary; created fresh at runtime by the preflight (heredoc), never hand-edited
in a shared file:

```json
{
  "hooks": {
    "Stop":              [{"hooks": [{"type": "command",
        "command": "/home/alexey/.aplexer-qual-f6ac6e7/bin/a state-report gated-idle || true"}]}],
    "Notification":      [{"hooks": [{"type": "command",
        "command": "/home/alexey/.aplexer-qual-f6ac6e7/bin/a state-report waiting || true"}]}],
    "UserPromptSubmit":  [{"hooks": [{"type": "command",
        "command": "/home/alexey/.aplexer-qual-f6ac6e7/bin/a state-report working || true"}]}],
    "SessionStart":      [{"hooks": [{"type": "command",
        "command": "/home/alexey/.aplexer-qual-f6ac6e7/bin/a state-report working || true"}]}]
  }
}
```

Deviation, documented: `CLAUDE_EVENTS[4..6]` (`awareness:claude` on
SessionStart/UserPromptSubmit/PostToolUse) are omitted — they install
`src/awareness.rs`'s context-injection source, not state-report commands, and
are not under qualification. Existing HOME/auth/shared settings stay
read-only; nothing runs `a init` anywhere; user-scope hooks are excluded at
launch by `--setting-sources project,local`.

## Preflight (every line `tee -a $QUAL/evidence/00-preflight.log`)

    export QUAL=/home/alexey/.aplexer-qual-f6ac6e7
    export APLEXER_STATE_DIR=$QUAL/state
    export APLEXER_RUNTIME_DIR=$QUAL/runtime
    export PATH="$QUAL/bin:$PATH"
    cd /home/alexey/git/aplexer
    sha256sum "$QUAL/bin/aplexer"          # == binding
    git merge-base --is-ancestor f6ac6e7 HEAD && echo pin-ancestor-ok
    git diff --quiet f6ac6e7 HEAD -- src Cargo.toml Cargo.lock && echo code-frozen-ok
    mkdir -p "$QUAL/work" "$QUAL/evidence" "$QUAL/work/.claude"
    cat > "$QUAL/work/.claude/settings.local.json" <<'JSON'
    <exact JSON block above>
    JSON
    sha256sum "$QUAL/work/.claude/settings.local.json"
    sha256sum ~/.claude/settings.json ~/.gemini/settings.json \
        ~/.local/bin/a > "$QUAL/evidence/shared-before.sha"
    cd "$QUAL/work" && git init -q .

## Sessions (engine argv carries the setting-source exclusion)

    a start --workspace "$QUAL/work" --tag qual --engine claude \
        -- claude --setting-sources project,local
    a start --workspace "$QUAL/work" --tag ctrl --engine claude \
        -- claude --setting-sources project,local

Record engine must be `claude` (`a status --json` both tags). All test
send/deliver runs INSIDE ctrl via `a send ... --enter` (recorded identity,
ancestor walk) — no bare-shell delivery.

## Fixture and controls

1. **Fresh owned README** (preflight writes `$QUAL/work/README.md`, ~120
   words, unique token `QUAL-RUN-<runid>` on its first line).
2. **Bounded real Working control (no permission round):** from outside,
   `printf '%s' 'Reply with exactly QUAL-TICK-<runid> and nothing else; do not
   use any tools.' | a send --workspace "$QUAL/work" --tag qual --stdin
   --enter`. While `a status --json` shows `working`, inside ctrl:
   `printf '%s' '<nonce text>' | a send --workspace "$QUAL/work" --tag ctrl
   --stdin --enter` (nonce text = `QUAL-NONCE-<runid>: when this message
   arrives, run: a message reply <MSG1_ID> "QUAL-ACK <runid>"`), then
   `a message deliver <MSG1_ID> --workspace "$QUAL/work"`.
   EXPECT: refusal `recipient reported working`; message stays queued;
   verbatim refusal + status captured. (No-tool task ⇒ no permission prompt ⇒
   no human round.)
3. **Ready boundary:** qual's own gated Stop (empty `background_tasks`)
   reports idle; `a status --json` shows fresh stamped `idle`. No
   state-report, TTL, quiet-PTY, or forced idle anywhere.
4. **Delivery + recipient-executed nonce ACK:** inside ctrl
   `a message deliver <MSG1_ID> --workspace "$QUAL/work"` (gate accepts
   reported idle); qual consumes it in a real turn and runs the instructed
   `a message reply <MSG1_ID> "QUAL-ACK <runid>"`. Evidence: reply envelope
   authored by qual with the verbatim nonce (`a message log`, `a message
   show`); ctrl inbox lists it; ctrl `a message ack` consumes it.
5. **Same-ID re-delivery refusal (no second PTY submission):** inside ctrl
   repeat `a message deliver <MSG1_ID> --workspace "$QUAL/work"`. EXPECT:
   refusal (nothing left to submit; verbatim error captured) and
   `a capture --screen --workspace "$QUAL/work" --tag qual` shows no second
   submission of the nonce.
6. **Waiting acceptance — NOT exercised.** It would require a human
   permission round (excluded by 01a122f8). Source guarantee stands at
   `message_deferred.rs:56` (`waiting` is a ready state); documented as
   unexercised.

## Evidence and cleanup

Every command `tee -a $QUAL/evidence/NN-*.log`. On ANY failure or unexpected
oracle: STOP and preserve `$QUAL` in full for root inspection. Green-run
cleanup only: `a kill --workspace "$QUAL/work" --tag qual`, same for `ctrl`;
isolated `a list` empty; re-sha the three shared files and
`diff` against `shared-before.sha` (must be empty); `rm -rf "$QUAL"` only on
root's release of the evidence.

## Limits (no absolute claims)

Residual engine-side delay before the hook process starts is unobservable
from the client (documented in source and reports); this recipe qualifies
observed behavior — it proves no "never". Hook firing is proven at runtime by
the event-stamped report shapes in captured status, not assumed.
