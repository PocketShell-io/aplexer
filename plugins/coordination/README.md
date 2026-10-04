# plugins/coordination — native coordination package sources

One shared protocol skill, six engine adapters. `scripts/package-coordination.py`
assembles these sources into per-engine bundles at a destination you choose;
the bundles are what engines consume. Nothing in this tree or in a generated
bundle is installed anywhere by default, and generation never edits user
config.

## Layout

- `engines/<engine>/` — adapter files for one engine, in the engine's native
  layout. `__APLEXER_BIN__` marks where the generator substitutes the
  absolute aplexer binary path into hook commands.
- The shared skill bundled into every package lives one level up in the
  repo: `.agents/skills/a2a-communication/SKILL.md` (the shared protocol source for these packages).

## Engine adapters

| Engine | Manifest | Hooks | Skill/context |
| --- | --- | --- | --- |
| `codex` | portable root `plugin.json` (`extensions.com.openai.hooks`) + `.codex-plugin/plugin.json` overlay | `hooks/hooks.json`, SessionStart + UserPromptSubmit + PostToolUse | `skills/` |
| `claude` | `.claude-plugin/plugin.json` | `hooks/hooks.json`, SessionStart + UserPromptSubmit + PostToolUse | `skills/` |
| `grok` | Claude-compatible layout | single `hooks/hooks.json` (PostToolUse; first-tool bootstrap) — one hook config, never a duplicate pair | `skills/` |
| `antigravity` | root `plugin.json` (name+description only) | `hooks.json` named-definition map (`PreInvocation` bootstrap) | `skills/` |
| `gemini` | `gemini-extension.json` | `hooks/hooks.json` (SessionStart, BeforeAgent, AfterTool) | `GEMINI.md` + `skills/` |
| `opencode` | none (single-file plugin) | `aplexer-awareness.js` (tool.execute.after; appends awareness after the original tool result, byte-for-byte) | `skills/` |

Generate every bundle into a temp directory:

```bash
scripts/package-coordination.py --engine all --dest /tmp/aplexer-coordination-bundles
```

See `docs/coordination-packages.md` for the full install/verification story
and the truthful limitations per engine.
