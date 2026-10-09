# Qualification recipe — gated Stop chain candidate `f6ac6e7` (rev 2)

**Status: FROZEN rev 2 for root review — NOT EXECUTED.** Rev 2 applies review
01a122ed corrections (prior 01ec6dd0 constraints stand): corrected waiting
oracle, genuine owned sender identity for delivery, project-scope hook config
with shared config strictly read-only, evidence preserved on failure. Rev 1
(d12b5b8) is superseded; this file replaces it.

## Oracles grounded in source (pin `f6ac6e7`)

- Readiness gate — `src/bin/aplexer/message_deferred.rs:56`: guarded
  delivery proceeds only when the recipient's state source is `"reported"`
  AND the state is `waiting` OR `idle`. Reported `running` refuses
  ("recipient reported working", line 74-75); missing/stale/derived
  readiness refuses with the full availability banner (line 65).
  THEREFORE: **working → refusal oracle; waiting → ACCEPTANCE oracle**
  (rev 1's waiting-refusal oracle was wrong — root 01a122ed is correct);
  idle → acceptance.
- Delivery authorization — `message_deferred.rs:13 authorize_delivery` with
  `src/process.rs:87 discover_session_id` (explicit `APLEXER_SESSION_ID` or
  the ancestor-environment walk): only the recorded **original sender**
  (calling session id == `message.from.session_id`, same source workspace)
  or the **recipient** (calling id == delivery recipient, same workspace)
  may deliver. An unrecorded controller shell CANNOT deliver with
  `--workspace` alone — rev 1's bare-shell delivery was invalid.

## Candidate binding (unchanged from rev 1)

- Source pin `f6ac6e78a2fe3b5d7a0b42255c84d1533a1dfe45`, clean tree at
  build; `cargo build --release` rc=0.
- Staged binary `/home/alexey/.aplexer-qual-f6ac6e7/bin/aplexer`, sha256
  `0ee40cea79c72a2c5615ed82a9557de763c053033fb88dacc92714b451edc2f0`
  (`$QUAL/bin/a` symlink; reports `a 0.1.10`). Hook commands embed this
  exact path (`resolve_a_bin` = `current_exe`, `src/hooks/mod.rs:262`).
- No shared install, no shared `a init`, no shared daemon/socket/state.

## Isolation (rev 2: shared config read-only, real auth untouched)

- Private registry/mailbox/socket: `APLEXER_STATE_DIR` / `APLEXER_RUNTIME_DIR`
  under `$QUAL` (unchanged).
- **Hooks via Claude's documented PROJECT scope** —
  `$QUAL/work/.claude/settings.json` — never user scope, never
  `CLAUDE_CONFIG_DIR`: rev 1's blank private config dir would have detached
  the engine from real auth, and copying credentials into it is forbidden.
  With real `$HOME` left alone, the engine's existing auth stays in place;
  project settings add only hooks. Grounding: `resolve_targets`
  (`src/hooks/mod.rs:297-332`) shows aplexer manages user-scope files only,
  and `src/api/start/` contains no hooks reference — start neither installs
  nor gates on hooks, so the engine picks hooks up from project scope at
  runtime.
- Derivation of the exact wiring (nothing hand-written): the preflight runs
  the staged installer against a THROWAWAY home and reuses its output
  verbatim:

      HOME="$QUAL/fakehome" "$QUAL/bin/a" init --engine claude

  `--engine claude` scopes the run to the claude settings target only (no
  shell rc / prompt targets, `cmd_init` prompt scoping), so the only file
  created is `$QUAL/fakehome/.claude/settings.json`. Its hook block is then
  copied verbatim into `$QUAL/work/.claude/settings.json`; both sha256s are
  recorded; `$QUAL/fakehome` is preserved as evidence (it contains no
  credentials — it was created empty for this one install).

## Preflight (evidence `$QUAL/evidence/00-preflight.log`, every step `tee -a`)

    export QUAL=/home/alexey/.aplexer-qual-f6ac6e7
    export APLEXER_STATE_DIR=$QUAL/state
    export APLEXER_RUNTIME_DIR=$QUAL/runtime
    export PATH="$QUAL/bin:$PATH"
    sha256sum "$QUAL/bin/aplexer"        # must equal the binding
    command -v a                         # must print $QUAL/bin/a
    git -C /home/alexey/git/aplexer rev-parse HEAD   # must equal the pin
    mkdir -p "$QUAL/work" "$QUAL/evidence" "$QUAL/fakehome"
    cd "$QUAL/work" && git init -q .
    HOME="$QUAL/fakehome" "$QUAL/bin/a" init --engine claude
    mkdir -p "$QUAL/work/.claude"
    cp "$QUAL/fakehome/.claude/settings.json" "$QUAL/work/.claude/settings.json"
    sha256sum "$QUAL/fakehome/.claude/settings.json" \\
        "$QUAL/work/.claude/settings.json"
    a engines                            # claude must be listed
    sha256sum ~/.claude/settings.json ~/.gemini/settings.json \\
        ~/.local/bin/a > "$QUAL/evidence/shared-before.sha"

## Sessions and identity (genuine owned sender context)

1. Recipient (flags unchanged from the verified rev-1 set):

       a start --workspace "$QUAL/work" --tag qual --engine claude \\
           --cwd "$QUAL/work" --startup-timeout-ms 30000

2. Controller/sender session, recorded in the same isolated registry:

       a start --workspace "$QUAL/work" --tag ctrl --engine claude \\
           --cwd "$QUAL/work" --startup-timeout-ms 30000

   Every `a message send` / `a message deliver` for the test is executed
   INSIDE ctrl, e.g.:

       a send --workspace "$QUAL/work" --tag ctrl \\
           "a message send --workspace $QUAL/work --to qual '<TEXT>'" --enter

   so the child process resolves ctrl's recorded identity through the
   ancestor-environment walk (`process.rs:87`) — no environment forging,
   no bare-shell delivery.

## Controls (rev 2 oracles)

3. **Working refusal**: give qual a genuine task
   (`a send --workspace "$QUAL/work" --tag qual "Summarize the first 50
   lines of README.md." --enter`); while `a status --json` shows working,
   inside ctrl send the nonce message and attempt delivery:

       a message send --workspace "$QUAL/work" --to qual \\
           "QUAL-NONCE-<runid>: when ready, reply by running: a message reply <MSG1_ID> \"QUAL-ACK <runid>\"" 
       a message deliver <MSG1_ID> --workspace "$QUAL/work"

   EXPECT: refusal ("recipient reported working"); message stays queued;
   capture refusal text + `a status --json`.
4. **Waiting acceptance (corrected oracle)**: with qual launched with
   permission prompts on (start flag as in rev 1) and sitting at a real
   permission prompt (`a status --json` shows waiting, source reported):
   inside ctrl run `a message deliver <MSG1_ID> --workspace "$QUAL/work"`.
   EXPECT: the gate ACCEPTS (per `message_deferred.rs:56`, waiting is a
   ready state); record the command's verbatim output. Skipped only with
   the reason recorded; never forced or faked.
5. **Idle acceptance + real consumption**: after qual's turn ends, its own
   gated Stop (`hook_event_name=Stop`, empty `background_tasks`) reports
   idle — verified via `a status --json` (fresh stamped event). Deliver
   then if not already delivered; qual consumes the nonce message in a
   real turn and executes the instructed reply
   (`a message reply <MSG1_ID> "QUAL-ACK <runid>"`) as the recipient.
6. **Recipient-executed ACK evidence**: `a message log --workspace
   "$QUAL/work"` contains the reply envelope authored by qual with the
   verbatim nonce; `a message show <REPLY_ID>`; ctrl's `a message inbox`
   lists it; `a message ack` marks it consumed. All ids verbatim.

`send`/`start`/`status` flags are exactly the rev-1 verified set, plus
`a init --engine claude` (verified against `InitArgs` in
`src/bin/aplexer/session_diagnostics.rs`).

## Evidence preservation (rev 2 — applies to every step)

Every command appends to `$QUAL/evidence/NN-*.log`. On ANY failure or
unexpected oracle: STOP immediately and PRESERVE `$QUAL` in full —
including `$QUAL/fakehome` — for root inspection. Cleanup below runs only
after a fully green run AND root's release; no `rm` of `$QUAL` ever happens
on a failed run.

## Cleanup (green run only, exactly owned state)

    a kill --workspace "$QUAL/work" --tag qual
    a kill --workspace "$QUAL/work" --tag ctrl
    a list    # isolated env: empty
    sha256sum ~/.claude/settings.json ~/.gemini/settings.json \\
        ~/.local/bin/a > "$QUAL/evidence/shared-after.sha"
    diff "$QUAL/evidence/shared-before.sha" "$QUAL/evidence/shared-after.sha" \\
        || echo "FAIL: shared state touched"
    rm -rf "$QUAL"   # only on root release of the evidence

## Documented limits (no absolute claims)

- Residual engine-side delay before the hook process starts remains
  unobservable from the client (documented in source and rev-1 report
  01a122eb); this recipe qualifies observed behavior, it proves no "never".
- Project-scope hook pickup by the engine is Claude's documented settings
  scope; step 5's captured state report (event-stamped shape from the
  staged binary) is the runtime proof the hooks actually fired.

## Freeze

Rev 2 frozen 2026-10-10 for root review; execution awaits explicit
authorization. Sole author: session 0dca91b2 (tag aplexer).
