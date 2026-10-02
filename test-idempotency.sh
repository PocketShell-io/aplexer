#!/bin/bash
set -euo pipefail

export APLEXER_WORKSPACE="$(pwd)/test-workspace"
rm -rf "$APLEXER_WORKSPACE"
mkdir -p "$APLEXER_WORKSPACE"

# Start a mock session to send to
cargo run -- start shell test-recipient &
PID=$!
sleep 2

# Send a message with idempotency key
cargo run -- message send --to test-recipient --idempotency-key "test-req-1" --json "hello world" > out1.json

# Send the same message with the same idempotency key
cargo run -- message send --to test-recipient --idempotency-key "test-req-1" --json "hello world" > out2.json

ID1=$(jq -r '.id' out1.json)
ID2=$(jq -r '.id' out2.json)

if [ "$ID1" == "$ID2" ]; then
    echo "Idempotency works: ID $ID1 == ID $ID2"
else
    echo "Idempotency failed: ID $ID1 != ID $ID2"
    exit 1
fi

cargo run -- kill test-recipient
echo "Test passed"
