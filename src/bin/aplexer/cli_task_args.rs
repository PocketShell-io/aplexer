use super::cli_examples::TASK_EXAMPLES;
use super::completions::{engine_completions, profile_completions};
use clap::{Args, Subcommand};
use std::path::PathBuf;

/// `a task` -- delegated-task verbs. One command today (`run`); the group
/// keeps future task surfaces (a lister, a re-router) from colliding with
/// session selectors.
#[derive(Args)]
pub(crate) struct TaskArgs {
    #[command(subcommand)]
    pub(crate) command: TaskCommand,
}

#[derive(Subcommand)]
pub(crate) enum TaskCommand {
    /// Run one noninteractive delegated task: prompt file in, engine exec
    /// out, evidence files and the real exit code left behind, and a
    /// durable completion notice sent as the calling session (never a faked
    /// identity). Host a task in a durable session with
    /// `a start -- a task run …`; this is the worker side.
    #[command(after_help = TASK_EXAMPLES)]
    Run(TaskRunArgs),
}

/// Arguments of `a task run` (see the `Run` variant above for the contract).
#[derive(Args)]
pub(crate) struct TaskRunArgs {
    /// File holding the prompt; its content is passed verbatim as the task
    /// child's final argument (no shell, no interpolation)
    #[arg(long, value_name = "FILE")]
    pub(crate) prompt_file: PathBuf,
    /// Engine id from `a engines`; defaults to the configured default engine
    /// (so an explicit `--engine` is how tasks are normally pinned)
    #[arg(long, add = engine_completions())]
    pub(crate) engine: Option<String>,
    /// Profile id from `a profiles` -- an engine variant (e.g. another
    /// account); preserves the requested account exactly like `a start`
    #[arg(long, add = profile_completions())]
    pub(crate) profile: Option<String>,
    /// Working directory for the task child (default: the current directory)
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
    /// Directory for START.json / RESULT.json / stdout.log / stderr.log
    /// (default: `<cwd>/.aplexer-tasks/<UTC stamp>-<engine>-<id>`)
    #[arg(long)]
    pub(crate) output_dir: Option<PathBuf>,
    /// Kill the task child's own process group and exit 124 after this many
    /// seconds. The kill never touches anything outside the child's group
    /// (0 never times out)
    #[arg(long)]
    pub(crate) timeout_secs: Option<u64>,
    /// Extra argv element inserted before the prompt (repeatable; e.g.
    /// `--engine-arg -c --engine-arg model_reasoning_effort=high` or
    /// `--engine-arg -o --engine-arg …/FINAL.md`). Passed through verbatim,
    /// never shell-interpolated
    #[arg(long = "engine-arg", value_name = "ARG")]
    pub(crate) engine_args: Vec<String>,
    /// Extra environment variable for the task child, KEY=VALUE (repeatable)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub(crate) env: Vec<String>,
    /// Keep the engine's confirmation/sandbox prompts. Default is to append
    /// the engine's skip-permissions argv (see `a start --help`)
    #[arg(long)]
    pub(crate) no_skip_permissions: bool,
    /// Tag the completion notice is addressed to in the notify workspace
    /// (default: `main`)
    #[arg(long, value_name = "TAG")]
    pub(crate) notify_to: Option<String>,
    /// Workspace the completion notice is delivered to (default: the calling
    /// session's own workspace; cross-workspace delivery requires the calling
    /// session record, exactly like `a message send --workspace`)
    #[arg(long, value_name = "PATH")]
    pub(crate) notify_workspace: Option<PathBuf>,
    /// Do not send a completion notice at all
    #[arg(long, conflicts_with_all = ["notify_to", "notify_workspace"])]
    pub(crate) no_notify: bool,
    /// RFC 3339 instant with an explicit UTC offset (e.g.
    /// `2026-10-04T03:00:00+02:00`): new launches after this instant use
    /// `--cutoff-engine` instead. Launch-time routing only -- nothing running
    /// is ever interrupted, and routing carries no application context
    /// (continuation needs the saved handoff file, not a fresh engine)
    #[arg(long, value_name = "RFC3339")]
    pub(crate) cutoff: Option<String>,
    /// Engine id tasks route to once `--cutoff` has passed
    #[arg(long, value_name = "ENGINE", add = engine_completions())]
    pub(crate) cutoff_engine: Option<String>,
    /// Run even if the output directory already holds a RESULT.json (a
    /// completed task is otherwise refused, so a re-run can't clobber it)
    #[arg(long)]
    pub(crate) overwrite: bool,
}
