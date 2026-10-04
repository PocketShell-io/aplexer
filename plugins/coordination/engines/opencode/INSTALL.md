# Installing aplexer-coordination (OpenCode)

Generated bundle — nothing was installed and no user config was touched.
`__APLEXER_BIN__` is the absolute aplexer binary baked into the plugin as a
JSON-serialized string literal; regenerate with
`scripts/package-coordination.py` if the binary moves. The bundle carries a
`.aplexer-generated-bundle.json` marker so `--force` replaces only bundles
this script produced.

## Layout

- `aplexer-awareness.js` — OpenCode plugin. On `tool.execute.after` it runs
  `__APLEXER_BIN__ context hook --engine opencode` (passing the tool name and the
  session id from `input.sessionID` or `APLEXER_SESSION_ID`) and appends any
  returned awareness text after the tool's model-facing `output.output` —
  the original tool result is preserved byte-for-byte.
- `skills/a2a-communication/SKILL.md` — the shared protocol skill.

## Install (optional, manual)

- Copy `aplexer-awareness.js` into your OpenCode global plugin directory
  (the same directory `a init` writes `aplexer-state-report.js` to — the two
  files are independent and may coexist), and place `skills/` where your
  build discovers skills. Config stays untouched.

## Verify

1. `a init --check --json` — state reporting (the other plugin file) is a
   separate concern.
2. In an OpenCode session inside an aplexer workspace: send yourself a
   message from a sibling (`a message send --to <your-tag> "ping"`), run any
   tool, and confirm the awareness text is appended to the tool output the
   model sees.
3. `a context --json` always shows the full picture on demand.

## Truthful limitations (v1)

- Injection rides `tool.execute.after` (the hooks worker's verified
  definition; output.output is mutable model-facing text). Tools whose
  results do not surface as `output.output` strings, and prompts with no
  tool calls, surface mail at your next checkpoint check instead.
- The payload identifies the session via `input.sessionID` or `APLEXER_SESSION_ID`; runs outside
  an aplexer session get an empty session id and no awareness text.
