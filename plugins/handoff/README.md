# plugins/handoff — the optional handoff-schedule package

An opt-in, removable add-on that restores the old fixed-time handoff
behavior (a daily launch, e.g. "03:00 Berlin to Antigravity") on top of the
native delegated-task support (`a task run`), without a daemon, a registry,
or global hooks.

## What it is

- **CLI verbs** (shipped in the aplexer binary itself):
  `a task handoff enable|disable|status|fire`. The wrapper is thin — a
  fire launches through `a task run`'s own code path, so engine/profile
  resolution, evidence records, the durable completion notice, exit codes,
  and cutoff routing are the native ones, not reimplementations.
- **One owned state directory**: `<aplexer state>/task-handoff/`
  (`schedule.json` written by `enable`, `fired/<slot>.json` claim markers).
  That directory is the plugin's entire footprint.
- **The package files in this directory**: documentation and an example
  schedule. They follow the repo's bundle philosophy (see the upstream
  `plugins/coordination/` on branches carrying commit bc0d3d7): plain
  files, nothing installed by default, enable never touches user config.

## What it is deliberately not

- **Not a scheduler daemon.** Nothing runs periodically by itself. The
  recurring trigger is whatever timer you already run — cron, a systemd user
  timer, a tmux ping — calling `a task handoff fire`. With no schedule, or
  after `disable`, fire is a successful no-op, so adding or removing the
  plugin never breaks an existing timer entry.
- **Not a host-engine plugin.** No coding agent loads schedulers, so this
  package cannot be a `plugins/coordination`-style engine bundle; it pairs
  the native CLI with your own timer. (Those coordination packages — see
  commit bc0d3d7 — remain the supported host-plugin packaging for awareness
  hooks: a separate concern.)
- **Not a context handoff.** A schedule decides *which engine a new launch
  uses* (optionally routed by a cutoff instant). A different engine starts
  fresh; carrying working context across engines is the prompt file's and
  `a handoff`'s (recovery-bundle) job. Routing and handoff are different
  things on purpose.

## Defaults

- No schedule exists until an explicit `a task handoff enable` runs:
  **absent by default**.
- Omitting `--engine` at enable time uses the **configured default engine at
  fire time** — the plugin never hardcodes or rewrites any engine,
  credential, or provider setting. Your default stays whatever your config
  says (e.g. `zcodex`) until you explicitly schedule otherwise.
- An engine cutoff, if you schedule one, lives only inside the owned
  schedule file and is removed with it.

See `INSTALL.md` (exact add steps) and `REMOVE.md` (exact removal steps and
the guarantees disable gives you).
