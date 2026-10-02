# Coordination packages

Native, distributable packages that give each supported coding agent the same
aplexer coordination surface: the a2a-communication protocol skill, hook
wiring for awareness context, and per-engine install notes. One shared skill,
six engine adapters, one generator.

- Shared skill (bundled verbatim into every package):
  `.agents/skills/a2a-communication/SKILL.md`
- Engine adapters: `plugins/coordination/engines/<engine>/`
- Generator: `scripts/package-coordination.py`

## Workspace awareness

Run `a context` before starting work. It lists nearby sessions and declared
visitors, their tasks and scopes, and related Git worktrees. These summaries
exclude session environment variables, command arguments, and message bodies.

```bash
a work join . --task "Update API routing" --mode edit --paths 'src/api/**'
a context --workspace /other/workspace --json
a work join /other/workspace --task "Review shared API" --mode review --paths 'src/**'
a message inbox --json
a work leave /other/workspace
```

Joining another workspace announces your participation without changing your
session UUID or moving its terminal. Default `message inbox` checks current
and declared workspaces and retains messages addressed to your UUID after a
move. `show`, `reply`, and `ack` follow those retained messages; old broadcasts
do not become new subscriptions. `message wait` watches the current routing
workspace only; use `inbox` at checkpoints to check the other mailboxes.

Declarations are advisory. Idle sessions retain their declarations; exited or
broken sessions' declarations are shown as stale. Overlapping edit scopes in
one checkout require an explicit peer agreement. Sibling Git worktrees are
related workspaces, but do not share the same files.

Native hooks remind an agent to inspect context, declare its work, and read,
reply to, and acknowledge unread messages. Repeated unchanged updates are
suppressed separately from message acknowledgements. Explicit tool paths into
another workspace trigger a destination reminder. Hooks do not wake idle
agents, infer ownership from shell command text, or enforce file locks.

## What a package contains

Every bundle carries:

- the shared `a2a-communication` protocol skill (identity, inbox etiquette,
  work declarations, dedup by message ID, peer mail as coordination);
- an awareness hook that runs `a context hook --engine <engine>` at a
  native lifecycle event, so unread sibling mail and active work declarations
  surface in the agent's context;
- an `INSTALL.md` with manual, optional install steps and a hook-ownership
  note.

Generation never installs anything, never edits user config, and emits no
secrets — the only configurable value is the absolute aplexer binary path
baked into hook commands.

## Try the local build

```bash
cargo build
./target/debug/aplexer context --json
scripts/package-coordination.py --engine all --dest /tmp/aplexer-coordination-bundles \
    --aplexer-bin "$PWD/target/debug/aplexer"
```

Use the full built binary path in agent commands until the installed `a`
includes the new commands. Login shells can reset `PATH`, so prepending a
local binary directory to a session's launch environment may not be enough.
Generated callbacks use the explicit absolute binary path.

## Generate

```bash
# all six engines into a temp destination
scripts/package-coordination.py --engine all --dest /tmp/aplexer-coordination-bundles

# one engine, explicit binary, machine-readable summary
scripts/package-coordination.py --engine claude --dest /tmp/bundles \
    --aplexer-bin /home/me/.local/bin/a --json
```

`--dest` is required: bundles land where you point. Generation is safe to
re-run: each bundle is built in a staging directory, validated, and renamed
into place atomically, and every bundle carries a
`.aplexer-generated-bundle.json` marker. `--force` replaces only directories
carrying that marker — never arbitrary pre-existing content. The generator
validates that every emitted JSON file parses, manifests keep their required
keys, the skill is intact, and no `__APLEXER_BIN__` placeholder survives.
The binary path is substituted structurally: JSON documents are parsed,
`shlex.quote`d into the command strings, and re-serialized; the OpenCode
plugin receives a JSON-serialized string literal — paths with spaces, quotes,
`$`, or backticks cannot break out into extra shell words.

## Per-engine layouts

| Engine | Manifest | Awareness hook events | Notes |
| --- | --- | --- | --- |
| `codex` | portable root `plugin.json` with `extensions.com.openai.hooks`, plus a `.codex-plugin/plugin.json` overlay for hosts predating the portable manifest | SessionStart (bootstrap), UserPromptSubmit, PostToolUse | hosts read root manifest or overlay, never both merged; bundled hooks load only after the user trusts them |
| `claude` | `.claude-plugin/plugin.json` | SessionStart (bootstrap), UserPromptSubmit, PostToolUse | plugin skills are namespaced (`aplexer-coordination:a2a-communication`); hooks are not namespaced and run in addition to settings.json hooks |
| `grok` | Claude-compatible layout (Grok consumes Claude-format hooks/skills) | PostToolUse (first-tool bootstrap) | startup hook output is ignored; exactly one hook configuration ships — Grok's personal hooks dir merges every `*.json`, so a second file would double-run |
| `antigravity` | root `plugin.json` (name + description only; the published schema rejects extra fields) | PreInvocation (bootstrap) | `hooks.json` is a named-definition map (`aplexer-awareness`), the same shape `a init` writes globally under a different key |
| `gemini` | `gemini-extension.json` (+ `GEMINI.md` context file) | SessionStart (bootstrap), BeforeAgent, AfterTool | `hooks/hooks.json` inner shape mirrors the nested format `a init` verifies in `~/.gemini/settings.json` |
| `opencode` | none — single-file plugin `aplexer-awareness.js` | tool.execute.after — appends awareness after the tool's model-facing `output.output`, original bytes preserved | see limitations below |

Sources verified against official docs (Codex plugin packaging,
Claude Code plugin create guide, Gemini CLI extension reference, Antigravity
plugins/hooks docs) and against this repo's own `a init` implementations in
`src/hooks/`, which carry the per-engine hook shapes aplexer already installs
and tests. Where an official page defers detail (Gemini's inner hook schema,
Antigravity's in-plugin hooks.json), we mirror the shape `a init` writes and
say so in the bundle's `INSTALL.md`.

## Install and verify (manual, optional)

The generator never installs. Each bundle's `INSTALL.md` has the per-engine
steps; the common shape is:

1. Generate into a temp directory.
2. Point the engine's plugin/extension installer at the bundle directory, or
   copy the files to the engine's plugin location (merge, never clobber
   existing hooks).
3. Verify:
   - `a init --check --json` — checks managed global hooks; it does not
     verify plugin loading or trust. Select one awareness owner as below.
   - From a sibling session: `a message send --to <your-tag> "ping"`, then
     trigger a tool call in the engine; awareness output (unread mail, work
     declarations) should appear.
   - `a context --json` shows the same picture on demand in any session.

## Hook ownership — one owner per concern

Use either the managed awareness hooks installed by `a init`, or the native
coordination package for that engine. Both invoke the same context callback.
Installing both duplicates callbacks, particularly startup bootstrap messages.

`a init` also installs state reporting (`a state-report`), preserves unrelated
hooks, and migrates its older inbox-only notice commands. Native coordination
packages supply awareness and the shared skill. If using a package with existing
settings hooks, retain state-report entries and remove only equivalent
`context hook`/legacy `message hook-notice` entries. Re-running `a init` restores
managed awareness; use that route alone when you want aplexer to manage the wiring.

Plugin hooks must be reviewed and trusted in the host. `a init --check` checks
managed settings files; it does not prove that a native plugin loaded or that
its hooks are trusted.

## Limitations

- **Session binding**: the first native conversation ID for each engine is
  bound to its aplexer session. A conflicting conversation gets no context.
  Launch independent agents in separate aplexer sessions instead of reusing
  one session for several native conversations.
- **Mailbox contention**: context and inbox snapshots use nonblocking locks.
  Native callbacks defer quietly when a mailbox is busy; an explicit inbox
  check can return a contention error for retry. Messages remain unread until
  explicitly acknowledged.

- **OpenCode**: injection rides `tool.execute.after` (appending to the tool's
  model-facing `output.output`, using the host's mutable tool output).
  Tools whose results do not surface as `output.output` strings, and prompts
  without tool calls, surface mail at the next checkpoint check instead; the
  session id comes from `input.sessionID` or `APLEXER_SESSION_ID`.
- **Codex**: root manifest and `.codex-plugin` overlay are alternatives, not
  layers — a host uses one and ignores the other. Bundled hooks stay inert
  until trusted.
- **Engine versions**: these adapters follow the current documented native
  schemas. If a host changes its schema, update its adapter and regenerate.
- **Awareness hook events** fire at session start (bootstrap on Claude,
  Codex, Gemini; first-tool bootstrap on Grok and OpenCode), then at tool boundaries (or turn boundaries for
  Gemini/Antigravity); mail arriving mid-turn surfaces at the next event,
  and a turn with no such events surfaces nothing until the next one.
- **`a context hook` output** requires the `context`/`work` CLI (the
  coordination command family). On builds without it, hooks exit 0 silently
  (`|| true`) and the skill's explicit `a context` guidance still applies.
- Packages are directory bundles, not marketplace uploads; zips/registries
  are out of scope for v1.

## Adapter references

- [Codex hooks](https://learn.chatgpt.com/docs/hooks) and [plugin manifests](https://developers.openai.com/plugins/build/plugins)
- [Claude Code plugins](https://code.claude.com/docs/en/plugins) and [hooks](https://code.claude.com/docs/en/hooks)
- [Grok hook guide](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/10-hooks.md)
- [Antigravity hooks](https://antigravity.google/docs/hooks)
- [Gemini CLI hooks](https://geminicli.com/docs/hooks/reference/)
- [OpenCode plugins](https://opencode.ai/docs/plugins/)
