# Non-disruptive shared CLI rollout proposal — candidate 53f535a2 (source 6dabc1ec)

**PROPOSAL ONLY — no step below has been executed.** ROOT acceptance required
(ref 01a12324: real gated-idle qualification of `6dabc1e`/`53f535a2` passed;
final ROOT proof review pending). Candidate staged at
`/home/alexey/.aplexer-fix-gated-idle/bin/aplexer`, sha256
`53f535a2512f3572e61b4a0058e2506ddaa8a25186cb89b5faf17afc3aceba02`,
`--version` -> `a 0.1.10`.

## A. Verified installed inventory (read-only audit, 2026-10-10)

| id | path | kind | sha256-8 | ver | role |
|----|------|------|----------|-----|------|
| T1 | `/home/alexey/.local/bin/aplexer` (`a` -> symlink to it) | native ELF | `5655521e` | 0.1.10 | **The reporter.** `~/.claude/settings.json` hooks call `/home/alexey/.local/bin/a state-report {working,waiting,idle}` on SessionStart/UserPromptSubmit/Notification/Stop for every Claude session. |
| T2 | `~/.local/share/uv/tools/pocketshell/lib/python3.14/site-packages/aplexer_cli/bin/aplexer` | native ELF | `790dfc64` | 0.1.9 | uv-tools `pocketshell` package bundle. The `uv-tools/pocketshell/bin/{a,aplexer}` console scripts are Python shims that exec this file — shims NOT replaced. |
| T3 | `~/git/pocketshell-cli{,-presence,-gateway-service,-windows-gateway}/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer` | native ELF | `0a4e6893` (x4 identical) | 0.1.8 | Per-repo venv bundles. Shims `.venv/bin/a` differ per repo (`732999d4` in pocketshell-cli; `f967c088`/`9da9fd11`/`eff2a8be` elsewhere) — shims NOT replaced. |
| T4 | `~/git/aplexer/python-cli/.venv` (shim `745ffb80`) | dev venv | — | — | This repo's dev-only venv. OUT OF SCOPE. |
| T5 | `/usr/bin/aplexer` | **deleted inode** | unavailable | unknown | The RUNNING supervisor + 43 live workers hold a deleted inode (exe resolves per /proc, file absent on disk, not dpkg-owned). No on-disk file to replace; NOT touched — no daemon restart. |

ROOT's named SHAs confirmed against disk: `565552`=T1, `732999`=pocketshell-cli
shim, `0a4e`=T3 bundles, `790dfc`=T2. Running processes are unreachable from
this rollout by construction: same-directory `rename(2)` never disturbs an
executing inode.

## B. Compatibility — old live workers / new reporter

- **Wire protocol**: candidate is the same 0.1.10 line; protocol magic and
  `Response.version` unchanged (`src/protocol.rs:56`), so the new client talks
  to the running supervisor cleanly.
- **Reports**: the new reporter adds `event_ms` and `engine_session_id`,
  both `#[serde(default)] Option` — `src/protocol.rs:180-193` states the
  design contract explicitly: an old client never sends them and **an old
  worker's serde ignores them**. New reporter -> old workers: accepted, no
  breakage.
- **Hook configs**: path-based and untouched on disk (no `a init`, no hooks
  reinit). Cached plain states (`idle`/`waiting`/`working`) parse unchanged;
  the `6dabc1e` ValueEnum alias additionally makes any `gated-idle` wiring
  parse — cached project contexts heal on binary swap alone.
- **What the swap does NOT do**: the running supervisor and all 43 workers
  keep their old in-memory code (deleted inode; worker spawn uses
  `current_exe()`, `src/worker/spawn.rs:103`, so even new workers inherit the
  old inode). The worker-side freshness fences (stamp ordering, engine-session
  fence, unstamped refusal) activate only after a root-authorized supervisor
  restart — explicitly out of scope here.
- **Stale-idle / later-PTY contradictions** (finished Fleet and this
  session): preserved as refusals — never faked, never bypassed. Each affected
  session resolves genuinely at its next real Stop, when the fixed reporter
  writes a fresh stamped report through the existing wiring.

## C. Atomic replace procedure (per target; independent, reversible)

Preflight (read-only): re-run `sha256sum` on every target and the candidate;
assert candidate == `53f535a2…`; parse probe
`env -u APLEXER_SESSION_ID <staged> state-report gated-idle </dev/null` ->
exit 1 with `gated-idle is wired for engine claude only` (proves the clap fix;
env-unset guarantees no write into the shared store).

For each target T1, T2, T3x4 (same directory, same filesystem — never
truncate-in-place, which would break a hook executing concurrently):

```sh
cp -p <path> <path>.bak-<old8>                      # backup, mode/owner kept
install -m 0755 /home/alexey/.aplexer-fix-gated-idle/bin/aplexer <dir>/.aplexer.new-53f535a2
mv -f <dir>/.aplexer.new-53f535a2 <path>            # atomic rename(2)
sha256sum <path>                                    # must print 53f535a2…
```

Order: T1 first (the load-bearing Claude reporter), then T2, then the four
T3s. Each target verifies independently; rollback is per-target. Backups stay
on disk until ROOT final acceptance; deleting them is a separate root-approved
step.

## D. Post-rollout verification (non-disruptive)

1. `sha256sum` all seven replaced paths == `53f535a2…`.
2. Parse probe at T1 through the `a` symlink (as in the preflight).
3. Read-only `a status` / `a message inbox` — running sessions unaffected.
4. The next natural Stop in any live Claude session writes a stamped report
   via the new code; old workers ignore the additive fields.

## E. Rollback

`mv -f <path>.bak-<old8> <path>` per target, then sha-verify against the old
hash. Rename-atomic in both directions; no downtime.

## F. Explicit non-goals (this proposal)

No supervisor/daemon restart; no `a init` / hooks reinit; no
`settings.json` edits; no session kills; no `/usr/bin/aplexer` restoration; no
`uv tool upgrade` / `uv pip install --reinstall` (either would clobber T2 —
documented hazard); no wheel publication; T4 untouched.
