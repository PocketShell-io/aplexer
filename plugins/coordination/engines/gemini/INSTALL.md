# Installing aplexer-coordination (Gemini CLI)

Generated bundle — nothing was installed and no user config was touched.
`__APLEXER_BIN__` is the absolute aplexer binary baked into hook commands
(spliced as a single shell word); regenerate with
`scripts/package-coordination.py` if the binary moves. The bundle carries a
`.aplexer-generated-bundle.json` marker so `--force` replaces only bundles
this script produced.

## Layout

- `gemini-extension.json` — extension manifest (`name`, `version`,
  `description`, `contextFileName` pointing at `GEMINI.md`).
- `GEMINI.md` — always-on context: identity, work declarations, inbox
  pointers.
- `hooks/hooks.json` — SessionStart (bootstrap), BeforeAgent, and
  AfterTool → `__APLEXER_BIN__ context hook --engine gemini`. The inner
  shape mirrors the nested hooks format aplexer's `a init` writes to
  `~/.gemini/settings.json`, with Gemini's event names.
- `skills/a2a-communication/SKILL.md` — the shared protocol skill.

## Install (optional, manual)

- `gemini extensions install <this-directory>` (or copy the bundle to
  `~/.gemini/extensions/aplexer-coordination/`), then confirm with
  `gemini extensions list`.

## Verify

1. `a init --check --json` — machine-wide state hooks in settings.json are checked separately; this does not verify plugin loading.
2. In a Gemini session inside an aplexer workspace: awareness output at
   session start, turn start, and after tool calls when you have unread mail or
   an active work declaration; `a context --json` on demand.
3. `a message send --to <your-tag> "ping"` from a sibling session.

## Hook ownership

Choose the native package or the managed awareness installed by `a init` for
this engine. Both use `context hook`; installing both duplicates callbacks.
Keep existing state-report hooks and unrelated hooks. If switching to a
package, remove only the equivalent global `context hook` and old managed
`message hook-notice` entries. `a init` restores managed awareness, so do not
re-run it for this engine while the package is the selected awareness owner.
