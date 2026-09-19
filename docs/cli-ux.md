# Terminal-first CLI UX

Status: partially implemented (PR #4); the rest of the doc remains proposed.

Aplexer has two equally important consumers:

1. PocketShell and other programs need stable, explicit, machine-readable contracts.
2. A person at a terminal needs fast orientation, low recall, truthful state, and safe recovery.

Those are not competing interfaces. The CLI should use the same session model for both, while rendering it differently according to context:

- `--json` is the stable integration surface.
- redirected human output remains plain and backwards-compatible.
- a real terminal may use color, relative time, concise summaries, aliases, and contextual next actions.

This document describes the jobs a terminal user hires aplexer to do, the current friction in those jobs, and the first implementation slice.

## Product model

Aplexer is not primarily a process list. It is a durable workspace for ongoing terminal work.

The important user concepts are:

- **workspace**: the repository or directory where work belongs;
- **session**: one durable shell or agent task in that workspace;
- **tag**: the human name for that task;
- **engine/profile**: how the task is launched;
- **lifecycle state**: starting, running, exiting, exited, failed, or broken;
- **agent state**: running (working), waiting for input, or idle;
- **activity**: recent PTY output, which is useful but is not the same as agent state.

The CLI should lead with those concepts. Worker PIDs, sockets, cgroups, and protocol details remain available for diagnosis but should not be the first thing a healthy user sees.

## Jobs to be done

### 1. Resume the right piece of work

**When I return to a terminal after a context switch, I want to see what is running by repository and task, so I can resume the right session without reconstructing identifiers.**

Success looks like:

- bare `a` gives a useful answer;
- the current directory is visibly marked;
- stable workspace/session numbers remain available for fast attach;
- stopped and broken sessions are distinguishable from healthy ones;
- the next attach command is visible, not remembered.

Current friction before this work:

- an empty registry prints nothing;
- every running session looks simply `running`, even when a fresh agent hook says it is waiting or idle;
- the current workspace is not called out;
- the footer teaches only the terse numeric syntax.

### 2. Start or reuse work in the current repository

**When I begin a task, I want one obvious command that starts the right shell or agent here, or resumes it when it already exists, so I do not need to remember launch plumbing.**

Success looks like:

- `a here` is the memorable path for create-or-attach in the current directory;
- engine and tag are optional, positional refinements: `a here codex review`;
- a full explicit path remains available: `a new --engine codex --tag review`;
- `a new` is the "always creates" counterpart: a fresh session in the current workspace, taking the next free `<tag>-2` suffix when the tag is already live, so adding a session to a workspace that already has one is one word (`start --fresh` is the same behavior on the machine contract);
- engine/profile discovery is one command away;
- rerunning the same intent resumes the existing live session.

The existing `a -` shortcut already has the right semantics, but `-` is an expert mnemonic rather than a discoverable verb. `a here` names the job without removing the compact form.

### 3. Know what needs attention

**When several agents are running, I want to distinguish working, waiting, idle, and dead sessions, so I spend attention where it is useful.**

Every state a CLI command prints comes from one unified eight-word vocabulary, each word with one definition:

| state | means |
|---|---|
| `starting` | the worker is coming up |
| `running` | doing work: the agent said so (fresh `a state-report` push), the PTY is producing output, or it is a plain shell that never reported anything |
| `idle` | alive and resting: the agent said so, or the PTY went quiet -- a silent compute step can look like this, so `idle` never means "blocked on you" |
| `waiting` | the agent said it is blocked and needs the user; never inferred from silence |
| `exiting` | a kill was accepted and teardown is running |
| `exited` | the workload ended; `a status` shows the exit code/signal |
| `failed` | the worker failed, or the workload died abnormally (OOM kill included; `a status` says which) |
| `broken` | the record claims alive but the worker process is gone; `a prune` reaps it |

One word per idea; where the word came from is a separate fact (`source`: `reported`, `activity`, or `lifecycle`), which `a status` qualifies ("inferred from output activity") and the status bar's spinner keys off. Honest language rules:

- `running`/`waiting`/`idle` are shown from a `state-report` push only while that push is authoritative;
- `idle` from a rest push stays authoritative until PTY output appears after it (beyond a small grace for the turn's tail racing the hook), not for a fixed clock window;
- recent PTY output and terminal silence alike map onto the shared `running`/`idle` words, marked as inferred;
- terminal silence alone must not be called `waiting` because a compute-heavy agent may be silent while still working;
- a plain shell that never reported agent state is `running` no matter how quiet; a shell an agent has lived in falls back to the same activity-derived `running`/`idle` words as a first-class engine once nothing is fresh;
- dead workers with an active persisted phase are `broken`, not `running`.

The distinction between **semantic state** and **activity heuristic** is part of the UI contract, not an implementation detail -- it lives in the `source` qualifier now, not in separate words. (`a watch`'s machine-readable `agent.state` events keep the raw heuristic words -- a silent terminal reads `waiting` there, with `metadata.state_source` telling reported from inferred -- for consumers that want the fine grain; the human UI maps that onto `idle` so silence never claims a wait.)

### 4. Stay oriented inside an attached session

**When I am inside a durable session, I want to know which workspace/task I am controlling, what it is doing, and how to leave or switch, so persistence never feels like being trapped in an opaque terminal.**

The status row should prioritize, in order:

1. semantic/lifecycle state;
2. task identity (tag, then workspace when space permits);
3. engine/profile and interesting foreground override;
4. sibling sessions and their numeric switch targets;
5. resource telemetry;
6. a persistent help affordance.

The row is adaptive rather than merely truncated. Wide terminals receive full context; narrow terminals retain state, tag, and `^b ?`.

`Ctrl-b ?` displays the live key reference in the row. Attach uses that same flash channel for the initial hint and switch failures, instead of writing a banner that the incoming screen snapshot immediately erases.

### 5. Switch or detach without damaging work

**When I need a different task, I want to switch or detach quickly while preserving both the remote workload and my local terminal state.**

Success looks like:

- `Ctrl-b Right/Left` (sessions), `Up/Down` (workspaces), `N/P`, `1-9`, and `l` switch in place, and `Ctrl-b n` creates another session here; `Ctrl-b s` and `Ctrl-b w` open picker boxes (this workspace's sessions; every workspace) where a digit attaches and Esc cancels;
- `Ctrl-b d` clearly means “leave this client, keep the session”;
- unknown `Ctrl-b` sequences are forwarded, preserving application input;
- terminal modes, scroll regions, cursor visibility, and alternate-screen state are restored;
- after cleanup, the CLI confirms whether the client detached or the session ended.

### 6. Diagnose and recover safely

**When a session is unhealthy or finished, I want a concise explanation and the next safe action, so I do not have to infer recovery from worker internals.**

Interactive `a status` should lead with task/state/workspace/engine, then foreground activity and process reachability. It should end with the next relevant command:

- attach a healthy session;
- inspect captured output for a stopped session;
- remove an obsolete record deliberately.

Deep details remain available in the same output and in JSON. A follow-up should add an explicit restart/reclaim flow for broken sessions rather than forcing users to compose `kill` and `start` themselves; this also addresses the recovery gap described in issue #1.

### 7. Automate the exact same model reliably

**When PocketShell or a script drives aplexer, it needs stable records, unambiguous errors, and byte-clean streams, so human presentation changes never break orchestration.**

Rules:

- do not add decoration to JSON;
- do not mix JSON metadata and terminal bytes;
- do not add ANSI sequences to redirected output;
- preserve exit codes and selector semantics;
- prefer additive fields and commands over reinterpretation;
- keep human aliases outside the machine contract unless explicitly promoted later.

The first slice therefore gates rich list/status rendering on a real stdout TTY and delegates non-TTY and `--json` paths to the existing implementation.

### 8. Coordinate related sessions

**When one agent needs context or an action from another, I want to address the recipient by workspace/task and inspect the conversation, so coordination survives client disconnects.**

The existing `a message` mailbox covers the durable primitive. The terminal UX follow-up is not a chat UI; it is lightweight awareness:

- show unread/awaiting-message counts in `a` and the attached status row;
- offer the exact inbox command;
- keep message contents out of the status row;
- do not inject unsolicited text into a program’s terminal by default.

## First implementation slice

PR #4 implements the highest-frequency, lowest-contract-risk improvements.

### Outside a session

- bare `a` on a TTY renders an empty state or an attention-first workspace tree;
- the current workspace is marked `here`;
- rows show semantic state when fresh and honest activity labels otherwise;
- relative recency is compact (`now`, `12s`, `5h 1m ago`, `5d 5h ago`);
- `a list --sort name|created|accessed|activity` reorders workspaces (remembered so `a N` matches the tree);
- long tags and engine/profile names are width-safe and use an ellipsis;
- the footer teaches attach, create-or-attach here, and help;
- `a status` on a TTY becomes a task-first summary with a suggested next action;
- top-level help adds a quick-workflow section;
- the human vocabulary is real Clap commands and visible aliases (so
  completions and `a help` know every name):
  - `a here` -> current-directory create-or-attach;
  - `a new` -> `start --attach --fresh` (always creates; `here` resumes);
  - `a open` -> `attach`;
  - `a ps` -> `list`;
  - `a current` -> `whoami`;
  - `a keys` -> `hotkeys`;
  - `a check` -> `doctor`;

### Inside a session

- the status row includes semantic/activity state;
- rendering degrades through full, medium, compact, and minimum layouts;
- `^b ?` remains visible as the help affordance;
- `Ctrl-b ?` flashes the complete key reference;
- attach confirmation uses the status row rather than stderr under the incoming snapshot;
- detach/session-end confirmation is printed only after terminal restoration.

### Compatibility boundary

- `a --json ...` is unchanged;
- `a snapshot` remains the stable full snapshot path;
- piped `a list` and `a status` retain the legacy text format;
- non-TTY attach delegates to the legacy byte-stream path;
- worker RPC, session records, terminal frames, and PocketShell integration commands are unchanged.

## Follow-up slices

The next work should remain job-shaped rather than become a collection of flags.

### Attention and recovery

- `a list --attention` for waiting, failed, broken, and OOM sessions;
- `a restart SESSION` with explicit semantics for finished versus broken records;
- `a reclaim SESSION` when a dead record still owns workspace/tag identity;
- recovery advice in `doctor` and `status` that chooses one of those verbs;
- a durability warning when the worker/workload is placed under a user-manager failure domain.

Shipped in this slice: the ack-gated crash warning. A session that dies
OOM-killed, fatally, or without any exit recorded leaves a warning sidecar
under the state root that every listing surface shows — as a banner under
the TTY `a list`, a per-row `warning` object in `a snapshot`/`a status
--json`, and a complete standalone `a warnings` list (`--json` for
machines). The warning outlives the session's record: `a prune` may reap
the record, the warning stays until an explicit `a ack` (bare, or targeted
by `workspace:tag` / UUID prefix / tag-in-workspace). Acknowledging moves
the sidecar under `acked/`, which both hides it and stops the query-time
sweep from re-materializing it while the broken record itself lingers.
Detection runs at query time from the same predicate the worker applies at
finalization, so a worker that died without being able to say so is still
caught by the next `a list`.

Shipped alongside it: PATH-independent engine resolution (issue #19). The
`engine_resolution` check probes every configured engine `command[0]` and
profile `executable` under both the invoking shell's PATH and a minimal
non-interactive session PATH (system dirs + `~/.local/bin`) — the
approximation of the app's SSH session where nvm and friends never load. A
bare name only the shell PATH resolves is warning-severity with
`a doctor --fix` as the fix: resolve via the current PATH, write the
absolute path into `config.toml` (comment-preserving, via toml_edit), and
re-check. Stale pins — absolute paths whose file moved on a version-manager
update — are flagged the same way and re-resolved from their basename. An
engine found under neither PATH is reported as not installed, which is a
legitimate state, not a defect.

### Launch confidence

- `a plan ...` as a human rendering of the existing launch resolution path;
- explain default engine/profile selection before an expensive launch when it is ambiguous;
- surface unavailable engine/profile fixes next to the error.

### Session awareness

- unread mailbox count, without message content, in list/status row;
- last meaningful activity rather than raw record-update time;
- optional `a list --current-workspace` and `a list --all-workspaces` shortcuts;
- one-shot `a wait SESSION --state waiting|idle|exited` for shell automation.

### Observability without overload

- `a status --verbose` for all worker/cgroup fields, leaving the default task-first;
- `a events SESSION` as a bounded human view over lifecycle/state changes;
- make heuristic versus reported state visible in verbose output;
- retain exact JSON fields as the source of truth for PocketShell.

## Evaluation

The terminal UX should be tested with tasks, not preferences:

1. From an empty registry, start a shell in the current directory without reading documentation.
2. With sessions in three repositories, attach the waiting review agent in under ten seconds.
3. From inside a session, discover detach and switch keys without leaving it.
4. Tell whether a silent agent is definitely waiting or merely idle.
5. After a workload exits, identify what happened and inspect its final screen.
6. Pipe list/status output into a script and verify byte-for-byte compatibility.
7. Run PocketShell contract tests against the same build.

The desired outcome is not fewer commands at any cost. It is less recall, faster orientation, truthful state, and a clear next action while preserving aplexer’s existing reliability and machine contracts.
