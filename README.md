<div align="center">

# aplexer

**Durable PTY sessions for coding agents - no daemon required.**

Run `claude`, `codex`, `gemini`, a plain shell, or any command. Sessions
keep running when you detach, survive a dropped connection, and stay
addressable by *project* and *name* from any terminal.

[![PyPI](https://img.shields.io/pypi/v/aplexer)](https://pypi.org/project/aplexer/)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue)](https://pypi.org/project/aplexer/)
[![CI](https://github.com/alexeygrigorev/aplexer/actions/workflows/ci.yml/badge.svg)](https://github.com/alexeygrigorev/aplexer/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux-fcc624?logo=linux&logoColor=black)](#requirements)

</div>

aplexer is a Linux-native, agent-aware alternative to tmux. Instead of
numbered panes on one server, you get one lightweight worker per session,
each with its own PTY, socket, and scrollback history. Sessions are grouped
by the directory they run in (the **workspace**) and named with a **tag**
you pick. You can cap them with cgroup-v2 resource limits, drive them
programmatically (`send`, `capture`, a Python client), and let them talk to
each other.

## Why aplexer

Five things it does differently:

- **Sessions outlive your terminal.** Detach, close the laptop, SSH back in
  from elsewhere - the workload is still there, scrollback intact.
- **Nothing to babysit.** No daemon, no server process. A worker exists only
  while its session does.
- **Built for agents, not just shells.** Sessions know which engine they run,
  report *working / waiting / idle* state via agent hooks, and can message
  sibling sessions in the same workspace.
- **Human *and* machine friendly.** Every command takes `--json`, and
  selectors like `myrepo:review` work the same for you and your scripts.
- **Optional resource isolation.** Give a runaway agent a 2 GB memory cap and
  a process limit with two flags.

## Requirements

You need:

- Linux (x86_64 or aarch64)
- Python 3.11+ for the pip install (the binaries are precompiled)
- Optional: a systemd user session with cgroup-v2 delegation for `--memory`
  / `--pids` / `--cpu-*` limits - everything else works without it

## Install

The package is on PyPI:

```bash
python -m pip install aplexer
```

That gives you three things:

| What | Where you'll see it |
|---|---|
| the `a` command | your everyday interface |
| the `aplexer` worker binary | the background process behind each session - `a` launches it, you never invoke it yourself |
| the `aplexer` Python package | `from aplexer import Client` |

<details>
<summary><strong>Install from source</strong> (Rust 1.85+)</summary>

```bash
git clone https://github.com/alexeygrigorev/aplexer
cd aplexer
cargo install --path .   # puts `a` and `aplexer` on your PATH
```

</details>

<details>
<summary><strong>Shell completions</strong></summary>

```bash
# bash: static script, or `source <(COMPLETE=bash a)` for live completions
a completions bash > ~/.local/share/bash-completion/completions/a

# zsh: restart the shell afterwards so compinit picks it up
a completions zsh > "${fpath[1]}/_a"

# fish
a completions fish > ~/.config/fish/completions/a.fish
```

</details>

## Quick start

Three commands cover the whole loop:

```bash
# 1. Start a session in the current directory (a shell, tagged "main")
a start

# 2. See every session, grouped by workspace
a list

# 3. Attach to it
a attach main
```

While attached, press **`Ctrl-b` then `d`** to detach - whatever was running
keeps running. That's the whole loop.

You can skip step 1 entirely. `a -` (read it as *"here"*) creates a session
on the spot and attaches to it, or attaches if one already exists. Close your
terminal, reopen it later, run `a -` again - you're back where you left off.

## Everyday shortcuts

aplexer's most common moves are one or two words long:

```bash
a                 # sessions at a glance (same as `a list`)
a -               # create-or-attach session "main" right here
a -review         # create-or-attach a session tagged "review" right here
a - codex review  # create-or-attach codex, tagged "review"

a 2               # attach to workspace #2 from `a list`
a 2 review        # attach to "review" in workspace #2
a open review     # attach by tag in the current workspace

a new             # always a fresh session here, attached
a new --engine codex --tag refactor
```

`a list` looks like this:

```text
[3] ~/git/dtc-website (◐ running 3/5)
├──  1  main           claude           ● running
├──  2  illustrations  codex            ● running
├──  3  make-run       shell            ● running
├──  4  layout         shell            ✗ broken
└──  5  clean-code     shell            ○ exited
```

Those bracketed numbers on the left are what `a 3` and `a 3 review` refer to.

## While attached

`Ctrl-b` is the prefix key, just like tmux, and these are the bindings you'll
actually use:

| Keys | Does |
|---|---|
| `Ctrl-b` `d` | detach - the workload keeps running |
| `Ctrl-b` `←` / `→` | previous / next session in this workspace |
| `Ctrl-b` `↑` / `↓` | previous / next workspace |
| `Ctrl-b` `1`–`9` | jump to the numbered session in the status bar |
| `Ctrl-b` `[` | scroll back through output (`q` or `Esc` to return) |
| `Ctrl-b` `n` | create another session in this workspace |
| `Ctrl-b` `R` | rename this session's tag |
| `Ctrl-b` `?` | show the full key reference on screen |

The mouse wheel scrolls back too (unless the running program wants the mouse),
and holding `Ctrl-b` briefly puts the whole cheat sheet on screen. Pressing
`Ctrl-b` twice sends one `Ctrl-b` straight through to the session and raises
nothing - which is how Claude Code's `Ctrl-b Ctrl-b` run-in-background chord
works here.

<details>
<summary><strong>Full key reference</strong></summary>

<!-- Keep in sync with `a keys` -->

```text
Right / Left  next / previous session in this workspace
Down / Up     next / previous workspace (at its most recent session)
n             create another session in this workspace and switch to it
s             list this workspace's sessions; 1-9 attaches, Esc cancels
w             list every workspace; 1-9 enters it, Esc cancels
d             detach (the workload keeps running)
[             scroll back through this session's output (i types, q/Esc leaves)
N / P         next / previous session across all workspaces
1-9           jump to the numbered session in the status bar
l             return to the previously attached session
r             redraw the live screen (recover a garbled display)
R             rename this session's tag (Enter confirms, Esc cancels)
?             show this reference in the status bar
```

In the scrollback pager, `PgUp`/`PgDn` or `Space` scrolls a screen at a time,
`k`/`j` or arrows a line at a time, `g`/`G` for top/bottom, `q` or `Esc` back
to live. Keys never reach the session while paging - press `i` to send your
typing to the session anyway. Scrollback defaults to 2000 lines
(`APLEXER_HISTORY_LIMIT`). `APLEXER_MOUSE=off` gives mouse selection back to
your terminal.

</details>

## Driving sessions without attaching

Anything you can do attached, you can do scripted:

```bash
# type into a session (any selector works: tag, workspace:tag, or UUID prefix)
a send review "cargo test" --enter

# peek at what a session shows right now
a capture review --screen

# stream a file into a session
a send review --stdin < patch.diff

# phase, exit info, liveness
a status review
```

## Session lifecycle

The lifecycle is four commands:

```bash
a kill review        # signal the workload and clean up its records
a forget <selector>  # drop records of a dead session you don't care about
a prune              # remove all dead, unreclaimable session records
a rename review --tag blocked   # change a session's tag
```

A session is addressed as a UUID (or prefix), a `workspace:tag` pair, or a
bare tag in the current workspace. When the tag is already alive, what
happens next depends on how you ask. `a -` attaches to the existing session,
`a new` claims the next free `<tag>-2` suffix, and plain `a start` demands
the exact tag and fails when it's taken.

### Crash warnings

A session that is OOM-killed or crashes leaves a **warning** that shows in
`a list` (and `a snapshot`/`a status` JSON) and stays there until you
acknowledge it — even if `a prune` has since removed the session's record:

```bash
a warnings           # list unacknowledged crash/OOM warnings
a ack                # clear them all
a ack myrepo:review  # clear one (works after the record is pruned)
```

## Engines, profiles & configuration

An **engine** is a command template: a coding agent like `claude`, `codex`,
`gemini`, `grok`, or `opencode`, or the plain `shell`. aplexer discovers
agents on your `PATH` automatically - run `a engines` to see what it found.
A **profile** is a named variant of an engine (another account, another
config), listed with `a profiles`.

Tweak any of this in `~/.config/aplexer/config.toml`:

```toml
version = 1
default_engine = "shell"

[engines.shell]
command = ["/bin/bash", "-l"]

[profiles.large]
engine = "shell"
history_bytes = 8388608          # 8 MiB of scrollback for `a capture`

[profiles.large.limits]
memory_bytes = 2147483648        # 2 GiB
pids = 256
```

Then use it: `a start --profile large`.

## Resource limits

On hosts with cgroup-v2 delegation (a normal systemd desktop or server),
single flags turn into real caps:

```bash
a start --memory 512M --pids 100
a start --engine codex --memory 2G
```

Check that your environment supports this with `a doctor` - it verifies the
cgroup controls end to end and tells you exactly what's missing if not.

## Agent awareness

aplexer knows more than "the process is alive":

```bash
a init        # one-time: install hooks so supported agents report working/waiting/idle
a init --check
```

With hooks installed, `a list` and the machine-readable event stream reflect
what each agent is actually doing - grinding away, waiting on you, or idle.

Every state aplexer prints is one of eight words, each with one meaning:

| state | means |
|---|---|
| `starting` | the worker is coming up |
| `running` | doing work - the agent said so, the terminal is producing output, or it is a plain shell that never reported anything |
| `idle` | alive and resting - the agent said so, or the terminal went quiet; a silent compute step can look like this |
| `waiting` | the agent said it is blocked and needs you; never guessed from silence |
| `exiting` | a kill was accepted and teardown is running |
| `exited` | the workload ended; `a status` shows the exit code |
| `failed` | the worker failed, or the workload died abnormally (OOM included; `a status` says which) |
| `broken` | the record says alive but the worker process is gone; `a prune` reaps it |

```bash
a whoami                    # your session's identity (workspace/tag/engine/profile)
a transcript review         # read a session's conversation transcript
a transcript review --follow
a transcript zoom --engine zcodex --path /path/to/rollout.jsonl
a watch --jsonl             # stream lifecycle events as they happen
```

For an agent started inside a plain shell session, pass its native JSONL file
with `--path` and its engine with `--engine`. The explicit file takes priority
over automatic discovery and any saved transcript binding. It applies only to
that invocation; repeat `--path` for later pages or `--follow`. The saved
binding is not changed.

## Messaging between sessions

Sibling sessions in a workspace share a durable inbox - handy when one agent
needs to hand off to another:

```bash
a message send --to review "done, see api.md"   # note to one sibling, by tag
a message send --all "standup in 5"             # every sibling in the workspace
a message inbox                                 # what's unread for this session
a message log                                   # the whole workspace conversation
```

## From Python

The `aplexer` package (installed alongside the CLI) talks to the same
sessions:

```python
from aplexer import Client

a = Client()
a.start(engine="claude", tag="review", workspace="/home/me/git/api")
a.send("api:review", b"please review the diff")
print(a.capture("api:review").decode(errors="replace"))
```

`Client` covers `start`, `send`, `capture`, `status`, `list`, `kill`, and
`forget`, with the same selectors as the CLI.

<details>
<summary><strong>Full command reference</strong></summary>

| Command | What it does |
|---|---|
| `a start` | start a session and its worker (full flag surface: engine, profile, limits, env, …) |
| `a new` | always create a fresh session and attach |
| `a here` / `a -` | create-or-attach in the current workspace |
| `a list` / `ls` / `ps` | sessions grouped by workspace |
| `a snapshot` | `list`, always machine-readable |
| `a attach` / `open` | attach to a session's live PTY |
| `a send` | type into a session without attaching |
| `a capture` | print captured output or the rendered screen |
| `a status` / `show` | phase, exit info, liveness |
| `a kill` | signal a session's workload and clean up |
| `a forget` | drop a dead session's records |
| `a prune` | remove all dead, unreclaimable records and their history |
| `a warnings` | list unacknowledged crash/OOM warnings |
| `a ack` | acknowledge crash/OOM warnings so they stop showing |
| `a rename` | change a session's tag |
| `a engines` / `a profiles` | list configured or discovered engines / profiles |
| `a doctor` | check the environment and config for problems |
| `a doctor --fix` | pin engine executables that only your shell's PATH can resolve to absolute config paths |
| `a init` | install or remove agent-state hooks |
| `a whoami` | print the current session's identity |
| `a state-report` | push agent state (used by the hooks `a init` installs) |
| `a message` | send / read messages between sibling sessions |
| `a watch` | stream session lifecycle events (`--jsonl`) |
| `a transcript` | read or follow a session's transcript |
| `a completions` | print a shell completion script |
| `a hotkeys` / `keys` | print the attach-mode key bindings |

Every command accepts `--json`, and every subcommand's `--help` ends with
examples.

</details>

<details>
<summary><strong>Environment variables</strong></summary>

| Variable | Meaning | Default |
|---|---|---|
| `APLEXER_CONFIG` | config file location | `~/.config/aplexer/config.toml` |
| `APLEXER_RUNTIME_DIR` | sockets and runtime state | `$XDG_RUNTIME_DIR/aplexer`, else `/tmp/aplexer-<uid>` |
| `APLEXER_STATE_DIR` | durable records, history, transcripts | `~/.local/state/aplexer` |
| `APLEXER_HISTORY_LIMIT` | scrollback lines in attach mode | `2000` |
| `APLEXER_MOUSE` | `off` leaves mouse events to your terminal | mouse enabled |
| `APLEXER_LAUNCH_SYSTEM_SCOPE` | `system` launches workloads in a system scope, immune to user-session teardown | per-user scope |

Inside a session, `APLEXER_SESSION_ID`, `APLEXER_WORKSPACE`, `APLEXER_TAG`,
and `APLEXER_WORKER` identify it to tools like `a whoami`.

</details>

## Troubleshooting

Start with `a doctor`, which checks the runtime and state directories, socket
paths, cgroup-v2 delegation, config, and session records. When something
fails, it suggests the fix (most commonly `a prune` for stale records).

**"command is not executable or was not found in PATH" from the app, but it
works in your terminal?** Your agent CLI (codex, claude, gemini, opencode)
resolves through a version manager (nvm and friends) that only interactive
shells load - the app drives `a start` over non-interactive SSH, which does
not. `a doctor` flags every engine that only your shell's PATH can resolve,
and `a doctor --fix` pins the resolved absolute paths into your
`config.toml`, so launches stop depending on the invoking shell's PATH. It
also re-resolves pins whose file has since moved (a version-manager update,
say).

## Development

```bash
cargo test                     # Rust unit + integration tests
scripts/validate.sh            # the full CI gate (fmt, clippy, tests, Python)
uv run --frozen --with pytest python -m pytest -q   # from python/ or python-cli/
```

The Rust core lives in `src/`, the thin Python CLI wrapper in
`python-cli/`, and the PyO3 bindings in `python/`.

## License

Licensed under Apache-2.0 - see [LICENSE](LICENSE).
