#!/bin/bash
set -euo pipefail

export APLEXER_WORKSPACE="$(pwd)/test-workspace-dup"
rm -rf "$APLEXER_WORKSPACE"
mkdir -p "$APLEXER_WORKSPACE"

# Send first time with idempotency key
cargo run -- message send --to test-recipient --queue --idempotency-key "req-2024-X" --json "hello world" > out1.json

# Send second time with same key
cargo run -- message send --to test-recipient --queue --idempotency-key "req-2024-X" --json "hello world 2" > out2.json

ID1=$(jq -r '.id' out1.json)
ID2=$(jq -r '.id' out2.json)

if [ "$ID1" == "$ID2" ]; then
    echo "Idempotency duplicate test works: ID $ID1 == ID $ID2"
else
    echo "Idempotency duplicate test failed: ID $ID1 != ID $ID2"
    exit 1
fi
