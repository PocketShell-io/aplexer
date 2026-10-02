# Installing aplexer-coordination (Grok, Claude-compatible)

Grok consumes Claude-compatible hooks and skills, so this bundle ships the
Claude plugin layout with `--engine grok` in the hook command. Exactly one
hook configuration ships (no duplicate copies that a merged hooks directory
could load twice). Nothing was installed and no user config was touched.
`__APLEXER_BIN__` is the absolute aplexer binary baked into hook commands
(spliced as a single shell word); regenerate with
`scripts/package-coordination.py` if the binary moves. The bundle carries a
`.aplexer-generated-bundle.json` marker so `--force` replaces only bundles
this script produced.

## Layout

- `.claude-plugin/plugin.json`, `hooks/hooks.json`,
  `skills/a2a-communication/SKILL.md` — the Claude-compatible set.
- `hooks/hooks.json` — PostToolUse (including the first-tool bootstrap) →
  `__APLEXER_BIN__ context hook --engine grok`. Grok's personal hooks
  directory merges every `*.json` file, so this one file (under any name) is
  the only hook config you place there.

## Install (optional, manual)

- If your Grok build supports Claude-style plugins, point it at this
  directory as with a Claude plugin.
- Otherwise copy `hooks/hooks.json`'s content into the hooks file your
  Grok build reads (e.g. a file in `~/.grok/hooks/`) and `skills/` into the
  skills location your build scans. Merge; never replace files you did not
  write. Install the hook wiring exactly once — from this bundle or from a
  hand edit, never both.

## Verify

1. `a init --check --json` — checks managed global hooks, including awareness; it does
   not verify that this package is loaded or trusted.
2. In a Grok session inside an aplexer workspace: awareness output at
   the first tool call and after later tool calls when you have unread mail or an active
   declaration; `a context --json` on demand.
3. `a message send --to <your-tag> "ping"` from a sibling, then trigger a
   tool call.

## Hook ownership

Choose the native package or the managed awareness installed by `a init` for
this engine. Both use `context hook`; installing both duplicates callbacks.
Keep existing state-report hooks and unrelated hooks. If switching to a
package, remove only the equivalent global `context hook` and old managed
`message hook-notice` entries. `a init` restores managed awareness, so do not
re-run it for this engine while the package is the selected awareness owner.
