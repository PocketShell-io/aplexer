#!/usr/bin/env bash
# test-idempotency-boundaries.sh
# Behavioral boundary verification for aplexer idempotency (B1 tag-reuse & B2 post-GC).
#
# B1 (Tag-Reuse Boundary):
#   - Session terminates/dies; fresh session resumes same tag (new session UUID).
#   - Replay of identical key + payload dedups against existing message & inherits original ID.
#   - Replay of identical key with ALTERED body/data fails with strict 409 idempotency conflict.
#   - Send of identical key from a DIFFERENT tag mints a new message ID (no cross-tag pollution).
#
# B2 (Post-GC / Capacity-Pruning Boundary):
#   - Message sent with key K -> minted ID1.
#   - Envelope is evicted past retention cap (10 MiB quota pressure / GC).
#   - message show ID1 fails (evicted from mailbox).
#   - Replay of key K mints a NEW ID2 != ID1 (proves duplicate downstream delivery boundary).
#
# All identity-sensitive operations run inside genuine bound sessions via PTY.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${APLEXER_BIN:-$REPO_DIR/target/debug/aplexer}"

if [ ! -x "$BIN" ]; then
    echo "ERROR: aplexer binary not executable at $BIN" >&2
    exit 1
fi

unset APLEXER_SESSION_ID
unset APLEXER_WORKSPACE
unset APLEXER_TAG

SCRATCH=$(mktemp -d /tmp/aplexer-b1b2-XXXXXX)
cleanup() {
    "$BIN" kill "$SCRATCH:worker-a" 2>/dev/null || true
    "$BIN" kill "$SCRATCH:worker-b" 2>/dev/null || true
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

run_in_session() {
    local sel="$1" cmd="$2" done_file="$SCRATCH/done_$RANDOM$RANDOM"
    "$BIN" send "$sel" "$cmd; echo __DONE__ > '$done_file'" --enter
    local waited=0
    while [ ! -f "$done_file" ]; do
        sleep 0.2; waited=$((waited + 1))
        if [ "$waited" -ge 120 ]; then
            echo "FAIL: timeout in $sel: $cmd" >&2
            return 1
        fi
    done
    rm -f "$done_file"
}

echo "=========================================================="
echo "=== Boundary B1: Tag-Reuse Behavioral Verification     ==="
echo "=========================================================="

echo "Starting sessions worker-a and worker-b in $SCRATCH..."
"$BIN" start --engine shell --workspace "$SCRATCH" --tag worker-a > /dev/null
"$BIN" start --engine shell --workspace "$SCRATCH" --tag worker-b > /dev/null
sleep 2

run_in_session "$SCRATCH:worker-a" "$BIN whoami --json > '$SCRATCH/whoami_a1.json'"
ID_S1=$(jq -r '.id' "$SCRATCH/whoami_a1.json")
echo "Session A1 established: $ID_S1 (tag: worker-a)"

# 1. Holder A sends K1
run_in_session "$SCRATCH:worker-a" \
    "$BIN message send --to worker-b --idempotency-key key-b1-reuse --data '{\"seq\":1}' --json 'payload b1 original' > '$SCRATCH/b1_orig.json' 2> '$SCRATCH/b1_orig.err'"
ID_B1_ORIG=$(jq -r '.id' "$SCRATCH/b1_orig.json")
[ -n "$ID_B1_ORIG" ] && [ "$ID_B1_ORIG" != "null" ] || { echo "FAIL: B1 original send failed"; cat "$SCRATCH/b1_orig.err" >&2; exit 1; }
echo "B1.1 Original message ID: $ID_B1_ORIG"

# 2. Kill holder A, start new session reusing tag worker-a
echo "Terminating session A1..."
"$BIN" kill "$SCRATCH:worker-a" > /dev/null
sleep 1

echo "Starting fresh session A2 with same tag 'worker-a'..."
"$BIN" start --engine shell --workspace "$SCRATCH" --tag worker-a > /dev/null
sleep 2

run_in_session "$SCRATCH:worker-a" "$BIN whoami --json > '$SCRATCH/whoami_a2.json'"
ID_S2=$(jq -r '.id' "$SCRATCH/whoami_a2.json")
echo "Session A2 established: $ID_S2 (tag: worker-a)"
[ "$ID_S1" != "$ID_S2" ] || { echo "FAIL: Expected fresh session UUID for A2, got identical $ID_S1" >&2; exit 1; }
echo "PASS B1.2: Confirmed distinct ephemeral session UUID ($ID_S2 != $ID_S1)"

# 3. Fresh session replaying same key + identical payload inherits original message ID
run_in_session "$SCRATCH:worker-a" \
    "$BIN message send --to worker-b --idempotency-key key-b1-reuse --data '{\"seq\":1}' --json 'payload b1 original' > '$SCRATCH/b1_replay.json' 2> '$SCRATCH/b1_replay.err'"
ID_B1_REPLAY=$(jq -r '.id' "$SCRATCH/b1_replay.json")
[ "$ID_B1_REPLAY" = "$ID_B1_ORIG" ] || { echo "FAIL: Replay under reused tag failed to inherit ID ($ID_B1_REPLAY != $ID_B1_ORIG)" >&2; exit 1; }
echo "PASS B1.3: Reused tag replay inherits stale message ID ($ID_B1_REPLAY == $ID_B1_ORIG)"

# 4. Fresh session replaying same key with ALTERED payload must trigger idempotency conflict
run_in_session "$SCRATCH:worker-a" \
    "$BIN message send --to worker-b --idempotency-key key-b1-reuse --data '{\"seq\":2}' --json 'payload b1 altered data' > '$SCRATCH/b1_conflict.json' 2> '$SCRATCH/b1_conflict.err' || true"
if ! grep -q "idempotency conflict" "$SCRATCH/b1_conflict.err"; then
    echo "FAIL: Expected idempotency conflict on payload mismatch under reused tag!" >&2
    cat "$SCRATCH/b1_conflict.err" >&2
    exit 1
fi
echo "PASS B1.4: Altered payload under reused tag triggered strict idempotency conflict"

# 5. Different tag replaying same key must mint a new message ID (no cross-tag pollution)
run_in_session "$SCRATCH:worker-b" \
    "$BIN message send --to worker-a --idempotency-key key-b1-reuse --data '{\"seq\":1}' --json 'payload b1 original' > '$SCRATCH/b1_diff_tag.json' 2> '$SCRATCH/b1_diff_tag.err'"
ID_B1_DIFF=$(jq -r '.id' "$SCRATCH/b1_diff_tag.json")
[ -n "$ID_B1_DIFF" ] && [ "$ID_B1_DIFF" != "null" ] || { echo "FAIL: worker-b send failed"; cat "$SCRATCH/b1_diff_tag.err" >&2; exit 1; }
[ "$ID_B1_DIFF" != "$ID_B1_ORIG" ] || { echo "FAIL: worker-b incorrectly deduplicated across tags!" >&2; exit 1; }
echo "PASS B1.5: Distinct tag mints independent message ID ($ID_B1_DIFF != $ID_B1_ORIG)"

echo "=== Boundary B1 Passed All Checks ==="
echo ""

echo "=========================================================="
echo "=== Boundary B2: Post-GC / Capacity Eviction Boundary  ==="
echo "=========================================================="

# 1. Holder A sends K2 -> ID_B2_ORIG
run_in_session "$SCRATCH:worker-a" \
    "$BIN message send --to worker-b --idempotency-key key-b2-evict --data '{\"test\":\"gc\"}' --json 'payload to be evicted' > '$SCRATCH/b2_orig.json' 2> '$SCRATCH/b2_orig.err'"
ID_B2_ORIG=$(jq -r '.id' "$SCRATCH/b2_orig.json")
[ -n "$ID_B2_ORIG" ] && [ "$ID_B2_ORIG" != "null" ] || { echo "FAIL: B2 original send failed"; cat "$SCRATCH/b2_orig.err" >&2; exit 1; }
echo "B2.1 Original message ID: $ID_B2_ORIG"

# Verify message is currently in mailbox
run_in_session "$SCRATCH:worker-a" "$BIN message show --json '$ID_B2_ORIG' > '$SCRATCH/b2_show_pre.json'"
[ -s "$SCRATCH/b2_show_pre.json" ] || { echo "FAIL: message show failed prior to eviction" >&2; exit 1; }
echo "Verified message $ID_B2_ORIG is stored in mailbox"

# 2. Push filler traffic past 10 MiB workspace quota cap from inside worker-b PTY
echo "Pumping filler messages past 10 MiB workspace limit..."
run_in_session "$SCRATCH:worker-b" \
    "for i in \$(seq 1 175); do P=\$(head -c 60000 /dev/urandom | base64 | head -c 60000); $BIN message send --to worker-a \"\$P\" >/dev/null 2>&1 || true; done"

# 3. Verify original message has been evicted (show fails)
if "$BIN" --workspace "$SCRATCH" message show --json "$ID_B2_ORIG" > /dev/null 2>&1; then
    echo "FAIL: Expected message $ID_B2_ORIG to be evicted, but message show still succeeded!" >&2
    exit 1
fi
echo "PASS B2.2: Message envelope $ID_B2_ORIG confirmed evicted from mailbox"

# 4. Replay key-b2-evict from worker-a with identical payload
echo "Replaying send with key-b2-evict after eviction..."
run_in_session "$SCRATCH:worker-a" \
    "$BIN message send --to worker-b --idempotency-key key-b2-evict --data '{\"test\":\"gc\"}' --json 'payload to be evicted' > '$SCRATCH/b2_post_gc.json' 2> '$SCRATCH/b2_post_gc.err'"
ID_B2_POST=$(jq -r '.id' "$SCRATCH/b2_post_gc.json")
[ -n "$ID_B2_POST" ] && [ "$ID_B2_POST" != "null" ] || { echo "FAIL: Post-eviction replay send failed"; cat "$SCRATCH/b2_post_gc.err" >&2; exit 1; }

echo "B2.3 Post-eviction message ID: $ID_B2_POST"
[ "$ID_B2_POST" != "$ID_B2_ORIG" ] || { echo "FAIL: Post-eviction replay wrongly reused evicted message ID!" >&2; exit 1; }
echo "PASS B2.4: Post-eviction replay minted a NEW message ID ($ID_B2_POST != $ID_B2_ORIG)"

echo ""
echo "=========================================================="
echo "=== ALL B1 AND B2 BEHAVIORAL BOUNDARY CHECKS PASSED    ==="
echo "=========================================================="
