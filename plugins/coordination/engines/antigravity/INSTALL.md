# Installing aplexer-coordination (Antigravity)

Generated bundle — nothing was installed and no user config was touched.
`__APLEXER_BIN__` is the absolute aplexer binary baked into hook commands
(spliced as a single shell word); regenerate with
`scripts/package-coordination.py` if the binary moves. The bundle carries a
`.aplexer-generated-bundle.json` marker so `--force` replaces only bundles
this script produced.

## Layout

- `plugin.json` — Antigravity plugin manifest (name + description only; the
  published schema rejects additional fields).
- `hooks.json` — named-definition map (`aplexer-awareness` →
  `PreInvocation`), the same shape aplexer's `a init` writes to the global
  `~/.gemini/config/hooks.json`, so global and plugin copies coexist as
  separate named definitions. PreInvocation is the bootstrap point, so
  awareness surfaces before the agent starts working.
- `skills/a2a-communication/SKILL.md` — the shared protocol skill.

## Install (optional, manual)

- `agy plugin install <this-directory>` (or `/plugin install` in the TUI),
  or copy the bundle to `~/.gemini/config/plugins/aplexer-coordination/`
  (global) or `.agents/plugins/aplexer-coordination/` (workspace-only).

## Verify

1. `a init --check --json` — machine-wide lifecycle/state hooks are a
   separate concern (different definition key: `aplexer-state-report`).
2. In an Antigravity session inside an aplexer workspace: awareness output
   before agent invocation when you have unread mail or an active work
   declaration; `a context --json` on demand.
3. `a message send --to <your-tag> "ping"` from a sibling session.

## Hook ownership

Choose the native package or the managed awareness installed by `a init` for
this engine. Both use `context hook`; installing both duplicates callbacks.
Keep existing state-report hooks and unrelated hooks. If switching to a
package, remove only the equivalent global `context hook` and old managed
`message hook-notice` entries. `a init` restores managed awareness, so do not
re-run it for this engine while the package is the selected awareness owner.
