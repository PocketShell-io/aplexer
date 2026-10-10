# Non-disruptive shared CLI rollout — candidate 53f535a2 (source 6dabc1ec), rev 3

**PROPOSAL + FROZEN ADMINISTRATIVE SCRIPT — nothing has been replaced.** ROOT
acceptance required before `apply`. Rev 3 supersedes rev 2 (9ddd315, kept as
the refused baseline) per ROOT journal findings (inbox 01a12347, mapped in
section G); rev 2 had superseded rev 1 (9476b27) per ROOT
review: exactly SIX named ELF targets, full-hash/uid/mode pins, O_EXCL +
O_NOFOLLOW tempfile discipline, shared-inode safety, corrected worker-launch
claim, no unconditional post-Stop claim. Wrapper:
`docs/rollout-53f535a2/apply_rollout.py` (stdlib-only python3;
`preflight` / `apply` / `rollback --manifest`).

Candidate: `/home/alexey/.aplexer-fix-gated-idle/bin/aplexer`
sha256 `53f535a2512f3572e61b4a0058e2506ddaa8a25186cb89b5faf17afc3aceba02`,
uid 1000. Read-only preflight (three PASS receipts, latest
`/home/alexey/.aplexer-rollout-53f535a2/preflight-4233d34debf5/`): all six
targets verified against the pins below; parse probe
`env -u APLEXER_SESSION_ID <candidate> state-report gated-idle </dev/null`
→ exit 1, stderr `a state-report: gated-idle is wired for engine claude only`
(clap fix proven, no state-store write possible).

## A. The six targets (preflight-verified 2026-10-10)

| id | full path | sha256 (current) | uid/gid | mode | inode | nlink |
|----|-----------|------------------|---------|------|-------|-------|
| T1 | /home/alexey/.local/bin/aplexer | 5655521e4881ad8e9d283434027361c0905f2f04e11a55c26ed7c473a3f1ad48 | 1000/1000 | 775 | 6314299 | 1 |
| T2 | /home/alexey/.local/share/uv/tools/pocketshell/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 790dfc6446a709499cbdf02a36d04eb3224eb352e55cbefd7937888e7cb69eb1 | 1000/1000 | 775 | 16254705 | 1 |
| T3a | /home/alexey/git/pocketshell-cli/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 0a4e6893e22429867c8d396711ecb526ad4e506627f3889dd7f6ee8969ff25ac | 1000/1000 | 775 | 11944740 | 1 |
| T3b | /home/alexey/git/pocketshell-cli-presence/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer | 0a4e6893… (same) | 1000/1000 | 775 | 14332318 | 5 |
| T3c | /home/alexey/git/pocketshell-cli-gateway-service/.venv/…/aplexer_cli/bin/aplexer | 0a4e6893… (same) | 1000/1000 | 775 | 14332318 | 5 |
| T3d | /home/alexey/git/pocketshell-cli-windows-gateway/.venv/…/aplexer_cli/bin/aplexer | 0a4e6893… (same) | 1000/1000 | 775 | 14332318 | 5 |

Out of scope, preserved untouched: every Python shim (`a`/`aplexer` console
scripts, e.g. pocketshell-cli shim 732999d4…), all hook configs, the dev venv
`~/git/aplexer/python-cli/.venv`, and the running daemon processes.

## B. Shared-inode safety (T3b/c/d = inode 14332318, nlink 5)

Three of the six names share one inode; two further (unnamed) links to the
same inode exist elsewhere (nlink 5). The wrapper NEVER opens a target for
writing and never hardlinks: backup and candidate are new inodes created in
the target's own directory, then `rename(2)` replaces exactly the one named
directory entry. The other links — named or not — keep the old inode and
bytes. Pre/post control: the preflight enumerates samefile links (bounded
sweep) and `apply` re-verifies after each T3 rename that the not-yet-named
siblings still hold inode 14332318 with sha 0a4e6893…, and post-apply that
every unnamed discovered link is unchanged.

## C. Per-target procedure (apply; stops at FIRST discrepancy)

For each target in order T1, T2, T3a, T3b, T3c, T3d, with a fresh per-run
nonce:

1. Re-assert target state: regular file, mode 775, uid 1000, sha == pinned
   original. Any drift → stop before touching anything.
2. Backup: same dir, `.<name>.orig-<nonce>`, created O_CREAT|O_EXCL|O_NOFOLLOW,
   mode 0775 (original preserved); bytes copied from the target
   (O_NOFOLLOW source), fsync, then verified: sha == original full sha, mode
   0775, uid 1000.
3. Candidate tempfile: same dir, `.<name>.new-<nonce>`,
   O_CREAT|O_EXCL|O_NOFOLLOW, mode 0755; candidate bytes copied, fsync,
   verified: sha == 53f535a2…, mode 0755, uid 1000, regular inode.
4. `rename(2)` tempfile over the target (atomic, same directory).
5. Post-rename verify: mode 0755, uid 1000, sha == candidate, inode == the
   tempfile's new inode (never a hardlink of the old one).

Backup stays 0775 (original); the installed entry is 0755 — a deliberate,
documented tightening. Backups and all receipts are retained under
`/home/alexey/.aplexer-rollout-53f535a2/` (manifest.json per run).

Rollback (automatic on failure; `rollback --manifest [--force]` manual): ONLY
targets committed by this run whose on-disk file still matches the recorded
installed identity (dev, inode, uid, mode) AND still hashes to the candidate
SHA are restored, from a backup re-verified by lstat against its recorded pin
(non-symlink regular, uid/gid, mode 0775, dev/ino, full original sha). A
target whose inode or content changed concurrently is never overwritten — it
is skipped and reported. Interrupted runs stay recoverable: the per-target
intent record (backup pin + install identity) is durable BEFORE each
rename(2), so `rollback --manifest --force` can conclude custody safely.

## D. Non-disruption controls (before/after, same run)

- Worker/mapped-image snapshot: every live process whose exe image mentions
  aplexer — pid, kernel birth time (/proc/<pid>/stat field 22), exe target,
  aplexer map-line count. Preflight PASS snapshot: 1226 processes
  (superset of the 43 shared native workers; 1181 execute the absent
  `/usr/bin/aplexer` inode — the paths are gone from disk per `ls`, the
  kernel readlink simply omits the " (deleted)" marker for these — plus 17
  holders of a previously-replaced T1 inode, which is exactly the
  rename-swap behavior this rollout relies on). `apply` refuses to finish
  unless this snapshot is byte-identical afterwards.
- Hook configs: sha256 of `~/.claude/settings.json` plus every
  `~/git/*/.claude/settings*.json` referencing `state-report`; identical
  before/after required.
- Unnamed hardlink siblings of 14332318: unchanged inode + sha required.

## E. Compatibility — corrected claims only

- Old live workers retain their old freshness behavior; the peer review found
  no ABI blocker for the six-target 53f535a2 rollout (same 0.1.10 protocol
  line; report fields `event_ms`/`engine_session_id` are `serde(default)`
  additive, ignored by old workers' serde — `src/protocol.rs:180-193`).
- REVISED (rev-1 claim withdrawn): worker launch resolves the worker
  executable via `APLEXER_WORKER` → `current_exe()` → sibling → PATH fallback
  (`src/process.rs:381-409`) and the workload environment prepends the binary
  directory to PATH (`src/worker/spawn.rs:102-118`). This rollout therefore
  makes NO claim about which code future workers execute; no
  worker-freshness deployment is claimed. The worker-side fences
  (stamp ordering, engine-session fence, unstamped refusal) reach live
  sessions only through a separate, root-authorized supervisor restart —
  explicitly out of scope here.
- REVISED (rev-1 claim withdrawn): after a live session's next real Stop, the
  fixed reporter writes a fresh stamped report through the existing wiring.
  The guarded decision may then legitimately conclude idle, working, or
  no-evidence — all remain valid outcomes/refusals. No unconditional
  next-Stop-idle claim is made; no idle is ever forced or faked.
- Hook configs are path-based: the rename(2) swap heals cached wiring with
  zero re-init. Already-deployed `gated-idle` spellings parse via the
  6dabc1e ValueEnum alias.

## F. Explicit non-goals

No supervisor/daemon restart; no `a init` / hooks reinit; no settings or
shim edits; no session kills; no `/usr/bin/aplexer` restoration or other
seventh path; no `uv tool upgrade` / reinstall (would clobber T2); no wheel
publication; no product source changes (this is an administrative fix only).
Nothing executes until ROOT review accepts this freeze.


---

## G. Rev 3 (corrected freeze, 2026-10-10) — mapping of ROOT journal findings

Rev 2 (9ddd315) is preserved untouched as the refused baseline. The six
targets, the candidate, and all non-goals are unchanged; only the wrapper
hardening below is new.

1. **Durable journal** (finding: manifest overwritten non-atomically, no
   fsync). Every manifest/receipt write is now a fresh `O_EXCL|O_NOFOLLOW`
   temp in the run dir, fchmod 0644, `fsync`, same-dir atomic rename,
   directory fsync. Each target's INTENT record — backup pin and the exact
   install identity — is durable BEFORE that target's `rename(2)`. An
   interrupted run (crash/SIGKILL mid-apply or mid-postcheck) always leaves
   backup custody and a recoverable transaction: `rollback --manifest
   --force` (non-finalized statuses are gated behind `--force`; finalized
   ones roll back directly).
2. **Installed-identity rollback** (finding: content-only comparison could
   clobber a concurrent same-sha replacement). Each committed entry records
   the installed identity (dev, inode, regular, uid, gid, mode, candidate
   sha) observed after its rename. Rollback restores only a target still
   matching that identity AND the candidate sha; a concurrent same-sha
   replacement (new inode) or any concurrent content change is skipped and
   reported, never overwritten.
3. **Backup pins** (finding: lstat validation incomplete). Backups are
   re-verified at restore time against the identity recorded at creation:
   non-symlink regular file, uid/gid, mode 0775, dev/ino, and the full
   original sha. Mismatch -> skip and report.
4. **Post-verify inside the transaction** (finding: postcheck failures
   escaped the scoped rollback). Post-verify now runs inside the `try`: any
   problem — including a failed `/proc/<pid>/stat|maps|exe` read or a
   process disappearing mid-check — takes the same first-discrepancy
   scoped-rollback path. The worker snapshot returns an explicit read-error
   list; preflight treats read errors as FAIL.
5. **Additional hardening found while correcting** (declared, not a ROOT
   finding): exact modes are enforced with `fchmod` because a umask can mask
   `os.open`'s mode argument (latent rev-2 defect — would have broken the
   0775 backup mode under umask 022); sources are opened `O_NOFOLLOW`; every
   created file is `fstat`-verified regular + owned before use; the wrapper
   records its own sha256 in every receipt/manifest for provenance.

Read-only preflight re-run with the rev-3 wrapper: PASS (receipt under
`/home/alexey/.aplexer-rollout-53f535a2/`, see freeze report). Still nothing
has been replaced; `apply` remains gated on ROOT acceptance of this freeze.
