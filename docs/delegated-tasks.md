# Delegated tasks (`a task run`)

One noninteractive delegated task, natively: a prompt file goes in, a
configured engine runs it to completion, the evidence lands in a directory,
and the calling session reports completion through the ordinary durable
mailbox — with its own identity, never a faked sender. This replaces the
per-project homemade orchestration glue (a Python runner, a config JSON, a
roster) with the same primitives aplexer already uses for sessions.

## The one command

```
a [--json] task run --prompt-file FILE [--engine E] [--profile P] [--cwd D]
    [--output-dir D] [--timeout-secs N] [--engine-arg A]... [--env K=V]...
    [--no-skip-permissions] [--notify-to TAG] [--notify-workspace W]
    [--no-notify] [--cutoff RFC3339] [--cutoff-engine E] [--overwrite]
```

- **Prompt argv preservation**: the prompt file's content is the task
  child's final argv element, verbatim — no shell, no interpolation. The
  START/RESULT records carry the prompt-free argv plus the prompt's byte
  count and SHA-256 instead of duplicating the text.
- **Engine/profile resolution is `a start`'s**: engine id, profile (another
  account), env, provider-key strip, skip-permissions argv. No engine
  aliases, no shell wrappers: the configured actual executable runs.
- **Noninteractive argv**: per engine, either the engine's `task_argv` in
  the config file or the built-in per-family default — codex family
  (`codex`, a configured `zcodex` fork) → `exec --json --skip-git-repo-check`;
  `antigravity` → `-p`; `claude`/`gemini` → `-p`; `opencode` → `run`.
  An engine with neither refuses instead of guessing (set
  `[engines.<id>] task_argv = ["-p"]` in the aplexer config to teach it).
- **Evidence** in `--output-dir` (default
  `<cwd>/.aplexer-tasks/<UTC-stamp>-<engine>-<id8>`): `START.json` written
  before the child spawns, `stdout.log`/`stderr.log`, and `RESULT.json`
  (atomic write) with the actual exit code, signal, timeout flag, resolved
  cwd/argv, prompt fingerprint, parent session, and notice outcome.
- **Exit code**: `a task run` exits with the child's actual code — a failed
  task can never look like a successful run to whatever hosted it. 124 on
  timeout; 127 when the child could not be launched at all (recorded in
  `RESULT.json` `error`).

## Parent association and the completion notice

There is no flag for "who is my parent" on purpose. `a task run` reads the
ambient `APLEXER_SESSION_ID` (walking ancestor environments when a tool
subprocess cleared it) and confirms it against a live session record; that
record is the task's `parent_session` and the *sender identity* of the
completion notice — the same rules as `a message send --workspace`: a real
session record is required, `--from` is never faked, an unknown target tag
is refused.

The notice is durable mailbox mail (`kind: task-result`) whose body names
the engine and the real status and whose `data` binds `task_id`,
`exit_code`, `output_dir`, `result_path`, and `parent_session`. Default
target: tag `main` in the calling session's own workspace; override with
`--notify-workspace`/`--notify-to`, or skip with `--no-notify`. A notice
problem never discards the task result: `RESULT.json` records
`sent`, `no-session-identity`, `identity-unresolved` (stale/foreign stamp),
`disabled`, or `failed` with the detail.

## Hosting a task in a durable session

`a task run` is a worker command, not a scheduler. Host it in a session and
you get durability, listing, and lineage for free — no parallel registry:

```
a --json start --workspace .tmp/case --tag fix-42 --engine shell -- \
  a task run --prompt-file .tmp/case/ROLE.md --engine zcodex \
             --cwd .tmp/case --timeout-secs 14400 \
             --notify-workspace ~/git/app --notify-to main
```

`a start` records the starting session as the new session's
`parent_session`, so the whole chain — root → host session → task result —
is real recorded lineage. Poll `a status <session>` / `a list`, read the
screen with `a capture`, and on natural exit the RESULT.json remains the
durable artifact.

## Timeout semantics

`--timeout-secs N` spawns the child in its own process group and, at the
deadline, SIGKILLs exactly that group and exits 124. Nothing outside the
group — the hosting session, its worker, any unrelated process — is ever a
target, and healthy runs are never killed to switch engines.

## Engine cutoff routing (timezone-aware)

`--cutoff <RFC3339 with explicit offset> --cutoff-engine <E>` routes
*launches only*: before the instant the requested engine runs, at/after it
the cutoff engine runs. The comparison is between absolute instants parsed
from the timestamp's own offset (`2026-10-04T03:00:00+02:00` equals
`2026-10-04T01:00:00Z`); a naive timestamp is rejected rather than silently
assumed local. A half-specified pair is an error.

Two things routing deliberately does **not** do:

- It does not touch running tasks. Switching engines happens for new
  launches; a healthy run always completes naturally.
- It does not carry application context. A different engine starts fresh;
  continuation across the cutoff is the caller's job — check
  `RESULT.json` (`exit_code == 0`, no `timed_out`) and the saved handoff
  file, then launch the follow-up with a prompt file that includes it.
  Don't imply that routing itself preserves context.

## Record schema

`TASK_RECORD_SCHEMA_VERSION = 1` (`src/task.rs`). `START.json` /
`RESULT.json` are additive versioned JSON documents; `--overwrite` is
required to reuse an output directory that already holds a `RESULT.json`,
so a completed task is never silently clobbered.
