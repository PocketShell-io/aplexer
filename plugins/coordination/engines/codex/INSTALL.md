# Installing aplexer-coordination (Codex)

Generated bundle — nothing was installed and no user config was touched.
`__APLEXER_BIN__` is the absolute aplexer binary baked into hook commands
(spliced as a single shell word); regenerate with
`scripts/package-coordination.py` if the binary moves. The bundle carries a
`.aplexer-generated-bundle.json` marker so regenerating with `--force`
replaces only bundles this script produced.

## Layout

- `plugin.json` — portable root manifest; `extensions.com.openai.hooks`
  points at `./hooks/hooks.json`.
- `.codex-plugin/plugin.json` — compatibility overlay for hosts that predate
  the portable manifest. A host reads one or the other, never both merged.
- `hooks/hooks.json` — SessionStart, UserPromptSubmit, and PostToolUse →
  `__APLEXER_BIN__ context hook --engine codex`.
- `skills/a2a-communication/SKILL.md` — the shared protocol skill.

## Install (optional, manual)

- Register the directory through a local plugin marketplace and install it
  through the host plugin manager (`/plugins`). CLI installation commands vary;
  check `codex plugin --help` rather than assuming an `install` subcommand.
  Keep the generated source directory available for the host to copy.
- Older hosts: copy `.codex-plugin/`, `hooks/`, and `skills/` into a plugin
  directory the host scans, or wire `hooks/hooks.json` content into
  `$CODEX_HOME/hooks.json` by hand — merge, do not clobber existing hooks.
  Bundled hooks stay inert until the host's trust prompt is accepted.

## Verify

1. `a init --check --json` — aplexer's machine-wide state hooks are checked separately; this does not verify plugin loading.
2. In a Codex session inside an aplexer workspace: awareness output should
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
