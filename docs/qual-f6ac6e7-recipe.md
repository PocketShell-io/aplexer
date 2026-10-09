# Qualification recipe — gated Stop chain candidate `f6ac6e7`

**Status: FROZEN for root review — NOT EXECUTED.** Author-owned, isolated,
real-CLI send → deliver → recipient-consumption qualification for the staged
candidate. Every command below is a plan; execution happens only after
explicit root authorization (message 01a122e4).

## Candidate binding

- Source pin: `f6ac6e78a2fe3b5d7a0b42255c84d1533a1dfe45`
  (chain `9cd07b0` → `2ab0860` → `583b803` → `f6ac6e7`; `git status
  --porcelain` empty at build time).
- Build: `cargo build --release` in `/home/alexey/git/aplexer`, rc=0 (the one
  warning is the pre-existing `doctor.rs:431 unused_mut`, file untouched by
  the chain).
- Staged binary: `/home/alexey/.aplexer-qual-f6ac6e7/bin/aplexer`
  sha256 `0ee40cea79c72a2c5615ed82a9557de763c053033fb88dacc92714b451edc2f0`
  (`$QUAL/bin/a` is a symlink to it; it reports `a 0.1.10`).
- Nothing is installed to `~/.local/bin/a` or any shared location; no shared
  `a init` is run; no shared daemon, socket, or state is touched.

## Isolation design

- All aplexer state (registry, mailbox, records, socket) lives under `$QUAL`
  via `APLEXER_STATE_DIR` / `APLEXER_RUNTIME_DIR` — the documented live-smoke
  isolation variables.
- Real engine hooks: `a start --engine claude` installs the candidate's hook
  wiring at start (start-time hooks check must pass). Claude reads hooks from
  `~/.claude/settings.json` plus `$CLAUDE_CONFIG_DIR/settings.json`
  (src/hooks/mod.rs:303); the session gets
  `CLAUDE_CONFIG_DIR=$QUAL/claude-config`, and hook commands are `a ...`
  resolved via `PATH` — the session launches with `PATH="$QUAL/bin:$PATH"`,
  so every hook executes the staged candidate, never the shared 0.1.10.
- Shared-state guard: sha256 of `~/.claude/settings.json`,
  `~/.gemini/settings.json`, and `~/.local/bin/a` recorded before and
  compared after; any difference FAILS the qualification.

## Preflight (evidence: `$QUAL/evidence/00-preflight.log`)

    export QUAL=/home/alexey/.aplexer-qual-f6ac6e7
    export APLEXER_STATE_DIR=$QUAL/state
    export APLEXER_RUNTIME_DIR=$QUAL/runtime
    export PATH="$QUAL/bin:$PATH"
    export CLAUDE_CONFIG_DIR=$QUAL/claude-config
    sha256sum "$QUAL/bin/aplexer"   # must equal the binding above
    command -v a                    # must print $QUAL/bin/a
    git -C /home/alexey/git/aplexer rev-parse HEAD   # must equal the pin
    mkdir -p "$QUAL/work" "$QUAL/evidence" "$QUAL/claude-config"
    cd "$QUAL/work" && git init -q .
    a engines                       # claude must be listed
    sha256sum ~/.claude/settings.json ~/.gemini/settings.json \
        ~/.local/bin/a > "$QUAL/evidence/shared-before.sha"

## Positive path (evidence: 10-*.log, 20-*.log, 30-*.log)

1. Start the recipient on a real engine:

       a start --workspace "$QUAL/work" --tag qual --engine claude \
           --cwd "$QUAL/work" --startup-timeout-ms 30000

   The start-time hooks check must pass for claude; record its output.

2. Baseline: `a status --workspace "$QUAL/work" --tag qual --json` — record
   the reported state and its stamp.

3. **Working control (real hook, refusal preserved):** give the engine a
   genuine task and let the UserPromptSubmit hook report working:

       a send --workspace "$QUAL/work" --tag qual \
           "Summarize the first 50 lines of README.md." --enter

   While `a status` shows working, queue a message and attempt guarded
   delivery:

       a message send --workspace "$QUAL/work" --to qual "QUAL-MSG-1 please ack" 
       a message deliver <MSG1_ID> --workspace "$QUAL/work"

   EXPECT: deliver REFUSES with the recipient reported working; the message
   stays queued untouched. Capture refusal text and `a status --json`.

4. **Ready boundary (genuine, never forced):** wait for the engine to finish
   its turn; its own gated Stop (hook_event_name=Stop, empty
   background_tasks, stop_hook_active absent) reports idle through the real
   hook. Verify `a status --json` shows idle with a fresh stamped event.
   No `a state-report`, no TTL, no quiet-PTY inference is used at any point.

5. **Guarded delivery + consumption:**

       a message deliver <MSG1_ID> --workspace "$QUAL/work"

   The guard passes only on the genuine idle; the message is submitted into
   the recipient PTY. The engine then consumes it in a real turn and replies:

       a message reply <MSG1_ID> "QUAL-ACK-1 received"

6. **Recipient ACK evidence:** the reply id exists in `a message log
   --workspace "$QUAL/work"`; `a message show <REPLY_ID>` shows
   from=qual; the controller's `a message inbox` lists the reply (unread →
   consumed after `a message ack <REPLY_ID>`). All ids recorded verbatim.

7. **Waiting control (optional, manual-gated):** a second tag `qual2`
   launched with `--no-skip-permissions` and a task that triggers a
   permission prompt; while the Notification hook reports waiting, `a
   message deliver` must refuse with waiting. Skipped only with reason
   recorded; never faked.

## Cleanup (exactly owned state)

    a kill --workspace "$QUAL/work" --tag qual     # and qual2 if launched
    a list                                          # isolated env: empty
    sha256sum ~/.claude/settings.json ~/.gemini/settings.json \
        ~/.local/bin/a > "$QUAL/evidence/shared-after.sha"
    diff "$QUAL/evidence/shared-before.sha" "$QUAL/evidence/shared-after.sha" \
        || echo "FAIL: shared state touched"
    # $QUAL removal happens only after root releases the evidence:
    rm -rf "$QUAL"

## Documented limits (no absolute claims)

- The residual race documented in the source stands: engine-side delay
  before the hook process starts is unobservable from the client; the
  process-start stamp is the closest available proxy. This recipe
  qualifies observed behavior; it does not prove "never".
- Hook commands resolve via `PATH`; step 2/3/4 evidence must therefore show
  the candidate's report shape (event-stamped) before delivery steps count.
- If any step errors, cleanup is still exactly the launched tag(s) killed
  plus `rm -rf "$QUAL"`; no shared-state repair is ever attempted.

## Freeze

Frozen 2026-10-10 for root review. Sole author: session 0dca91b2 (tag
aplexer). Awaiting explicit authorization before preflight step 1.
