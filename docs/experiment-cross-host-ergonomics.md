# Cross-Host & Ergonomics Design

## 1. Problem Statement
The experiment identified native orchestration ergonomics and cross-host addressing as painful, notably causing `orchestrator-channel.py` to suffer a crash-after-send duplicate window. We need discoverable logs/progress for headless workloads, accurate state, request/reply correlation, and authenticatable stable identity.

## 2. Architecture Decisions & Alternatives Evaluated

### Alternative 1: Full Message Broker (Rejected)
Building a fully-fledged broker daemon (e.g., RabbitMQ, Redis) across hosts would allow guaranteed cross-host delivery but violates the core principle of aplexer: "Preserve native durable mailbox semantics; do not create a competing production broker." 

### Chosen Architecture: Native Idempotency Keys (Revised)
We introduced the `--idempotency-key` argument directly into `aplexer message send` and `reply`. The key is persisted in `MessageEnvelope`. 

**Corrections made during follow-up:**
1. **Scope of Deduplication:** A key is now only valid for deduplication if both the **logical sender** (workspace and tag, ignoring ephemeral session IDs on restart) and **logical recipient** match perfectly. This prevents one sender from accidentally receiving the deduplicated success of a completely different sender using the same key.
2. **Payload Conflict Rejection:** If a message with the identical key exists, but the payload (`body`, `reply_to`, `kind`) differs, it is strictly rejected as a conflict.
3. **Atomicity:** The read-then-write list operation has been moved strictly inside the `FileLock::exclusive(&mailbox_lock_path(mp))` critical section in `store.rs`, ensuring no concurrent retries can race the mailbox.
4. **Reply Support:** Both `send` and `reply` correctly deduplicate because the check sits directly in the unified backend storage write layer (`write_message_idempotent`).
5. **Retention:** Garbage Collection rules (retention based on ack bounds and capacity caps) govern the deduplication window. Expired or GC'd messages cannot be deduplicated and will appear as new sends. 

## 3. Remote Host Addressing
**Correction:** The initial claim of "demonstrating a two-computer roundtrip via SSH" was inflated and inaccurate; sending a message to a locally running `muse-reviewer` via `aplexer message send` is merely a same-host dispatch. 

**True Cross-Computer Design:** 
A real two-computer dispatch involves the orchestrator on Desktop executing an SSH command on the remote host (Hetzner) targeting a bound remote session. E.g., `ssh user@host "aplexer message send --workspace /path --to target ..."`. 
The `idempotency_key` guarantees that if the SSH connection drops before the Desktop receives the `0 (success)` exit code, the Desktop can replay the identical SSH command, and the remote `aplexer` will return success via the native atomic deduplication, completely closing the crash-after-send window without needing broker infrastructure. 

## 4. Semantics & Tamper-Proof Consensus
**Correction:** Typed correlation fields (`reply_to`, `idempotency_key`) strictly establish request/reply routing geometry. They **do not** establish semantic agreement, approval, or tamper-proof consensus. Agents must independently verify the actual content or rely on signed payloads if cryptographic non-repudiation is required. The transport is merely reliable delivery.

## 5. Security & Addressing Quirks Discovered
- **Inherited-Child Mailbox Authority:** During testing, we verified that nested bash scripts natively inherit `APLEXER_SESSION_ID` from their parent agent (via `cgroup` or environment variable). This grants child scripts the full authority of the parent agent's mailbox identity. For isolated tests, we had to explicitly override this to prevent the script from spoofing the orchestrator/repair agent.
- **Expired Peer Routing:** `aplexer` correctly halts delivery with a "never existed" error if the target tag is completely unknown to the workspace history, unless the sender explicitly adds `--queue` to acknowledge they are parking a message for a future workload.
- **Native Addressing vs Remote Mailbox Access:** Native addressing (like `aplexer message send`) relies on the local disk `.local/state/aplexer`. A remote SSH orchestrator doesn't have direct disk access to `.local` via its own `aplexer` binary; it MUST run the `aplexer` CLI *on* the remote host through SSH. The idempotency key makes this safe.
