# Installing aplexer-coordination (Claude Code)

Generated bundle — nothing was installed and no user config was touched.
`__APLEXER_BIN__` is the absolute aplexer binary baked into hook commands
(spliced as a single shell word); regenerate with
`scripts/package-coordination.py` if the binary moves. The bundle carries a
`.aplexer-generated-bundle.json` marker so regenerating with `--force`
replaces only bundles this script produced.

## Layout

- `.claude-plugin/plugin.json` — manifest. Only `plugin.json` lives in
  `.claude-plugin/`; components stay at the bundle root.
- `hooks/hooks.json` — SessionStart, UserPromptSubmit, and PostToolUse →
  `__APLEXER_BIN__ context hook --engine claude`.
- `skills/a2a-communication/SKILL.md` — the shared protocol skill (invocable
  as `aplexer-coordination:a2a-communication` once installed).

## Install (optional, manual)

- `claude --plugin-dir <this-directory>` for a session-local trial, or copy
  the bundle into your plugins directory / a marketplace layout. The
  `hooks` key in `hooks/hooks.json` matches the settings-file hooks shape.

## Verify

1. `a init --check --json` — checks managed global hooks, including awareness; it does
   not verify that this package is loaded or trusted.
2. In a Claude session inside an aplexer workspace: awareness output should
   appear at session start, prompt submit, and after tool calls when you have
   unread mail or
   an active work declaration; `a context --json` shows the same on demand.
3. `a message send --to <your-tag> "ping"` from a sibling session, then
   trigger any tool call.

## Hook ownership

Choose the native package or the managed awareness installed by `a init` for
this engine. Both use `context hook`; installing both duplicates callbacks.
Keep existing state-report hooks and unrelated hooks. If switching to a
package, remove only the equivalent global `context hook` and old managed
`message hook-notice` entries. `a init` restores managed awareness, so do not
re-run it for this engine while the package is the selected awareness owner.
