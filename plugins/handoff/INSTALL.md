# Install (manual, opt-in — nothing is installed by default)

The verbs are part of the aplexer binary; the "plugin" is the opt-in
schedule you create plus (optionally) a timer entry in your own scheduler.
Use the full built binary path in timer commands until the installed `a`
includes `task handoff`.

## 1. Enable the schedule (this is the opt-in)

```bash
a task handoff enable --at 03:00 \
    --prompt-file /abs/path/ROLE-HANDOFF.md \
    --engine antigravity \
    --cwd /abs/path/workspace \
    --timeout-secs 14400 \
    --notify-workspace /abs/path/workspace --notify-to main
# `--engine` is optional: omit it to use the configured default engine at
# fire time (nothing here rewrites your default).
```

- `--at 03:00` means the machine's local 03:00 (DST-aware); omit `--at` to
  launch whenever `fire` runs instead (at most once per local minute).
- Writes exactly one file: `<state>/task-handoff/schedule.json`
  (`a task handoff status` shows the path). Nothing else changes — no
  engine config, no credentials, no hooks, no timer.
- Scheduling `--output-dir`/`--overwrite` is refused on purpose: every fire
  writes its own `.aplexer-tasks` evidence directory.

## 2. Add the trigger in your own scheduler (never done by aplexer)

`fire` decides due-ness itself (at most one launch per local day at/after
`--at`, claimed atomically), so a timer that runs more often than daily is
safe.

cron — daily at 03:00 (fire's window check makes this exact):

```
0 3 * * * /path/to/a task handoff fire
```

systemd user timer — hourly fire, one launch per day at most:

```ini
# ~/.config/systemd/user/aplexer-handoff.service
[Unit]
Description=aplexer handoff scheduled launch

[Service]
Type=oneshot
ExecStart=/path/to/a task handoff fire

# ~/.config/systemd/user/aplexer-handoff.timer
[Unit]
Description=hourly chance for the aplexer handoff schedule

[Timer]
OnCalendar=hourly
Persistent=true

[Install]
WantedBy=timers.target
```

`systemctl --user enable --now aplexer-handoff.timer`

A fire that runs inside an aplexer session (e.g. hosted with `a start --
… task handoff fire`) also records the real parent lineage and can send the
completion notice as that session; a bare cron fire works too and simply
records no parent (the notice skip is recorded honestly in RESULT.json).

## 3. Verify

```bash
a task handoff status           # schedule, next due, fired slots
a task handoff fire             # manual trigger; no-op when not due
```
