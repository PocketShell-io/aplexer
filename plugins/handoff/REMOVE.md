# Remove (clean, idempotent)

## 1. Remove the schedule (idempotent)

```bash
a task handoff disable          # already removed → success ("already disabled")
```

`disable` removes exactly one directory — `<state>/task-handoff/` (the
schedule file and the fired-slot markers) — and nothing else. Running it
again is a successful no-op.

## 2. Remove your timer entry (your own scheduler; aplexer never added one)

Delete the cron line or `systemctl --user disable --now aplexer-handoff.timer`
you added in INSTALL.md. This step is yours because the timer was always
yours; a leftover entry is harmless anyway (`fire` without a schedule exits
0 as a no-op).

## 3. Delete the package files

Remove this `plugins/handoff/` directory from the checkout. The verbs stay
in the binary but do nothing until an `enable` creates a schedule again.

## What removal guarantees

- **No live cancellation**: nothing is ever signaled. A launch that already
  started runs to natural completion and keeps its RESULT.json and notice;
  running agents, sessions, and workers are untouched.
- **No artifact deletion**: task evidence (`.aplexer-tasks/**`), worktrees,
  and prompts are never removed by disable — only the owned state directory
  is.
- **No unrelated timers touched**: aplexer owns no timers; your scheduler's
  other entries are unreachable from `disable`.
- **No credential/provider change**: enable/disable never write engine,
  profile, account, or key material. The configured default engine (e.g.
  `zcodex`) is exactly what plain `a task run` keeps using.
- **No automatic engine cutoff left active**: a cutoff existed only inside
  the removed schedule file (or as an explicit per-launch `a task run`
  flag, which removal never had). After disable there is no persisted
  cutoff anywhere.
- **Native tasks stay usable**: `a task run` never depended on the
  schedule; with the plugin "removed" (no schedule, no package files), every
  native prompt-file task works exactly as before.
