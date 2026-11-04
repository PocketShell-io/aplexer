# Native runtime deadlines

The optional --deadline-ms absolute Unix-millisecond start flag persists a hard
runtime deadline before spawning the worker. The worker arms a monotonic timer
before readiness, independent of the launching CLI, host, or network. Expired
deadlines refuse before claiming; startup rechecks before workload spawn.

At expiry it persists deadline_exceeded=true and invokes the existing whole
containment kill path, retrying errors until empty. Actual exit information is
preserved. Finished receipts retain deadline_ms, deadline_exceeded and verified
containment_empty even without retained terminal output. Early ordinary exit
has no exceeded marker. Existing starts without the flag remain unbounded.

Controllers must bind this absolute deadline to persisted admission and never
extend it on delivery retry. This is a hard termination boundary, not business
completion. If the worker itself is killed, existing containment recovery is
still responsible for that exceptional process failure.
