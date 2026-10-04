use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct ContextArgs {
    /// Directory whose collaborators to inspect (defaults to the current directory).
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: Option<ContextCommand>,
}

#[derive(Subcommand)]
pub(crate) enum ContextCommand {
    /// Internal lifecycle callback; native hook JSON arrives on stdin.
    #[command(hide = true)]
    Hook(ContextHookArgs),
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ContextEngine {
    Claude,
    Codex,
    Grok,
    Gemini,
    Antigravity,
    Opencode,
}

impl ContextEngine {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Args)]
pub(crate) struct ContextHookArgs {
    #[arg(long, value_enum)]
    pub(crate) engine: ContextEngine,
}

#[derive(Args)]
pub(crate) struct WorkArgs {
    #[command(subcommand)]
    pub(crate) command: WorkCommand,
}

#[derive(Subcommand)]
pub(crate) enum WorkCommand {
    /// Declare task and file scope before working in a workspace.
    Join(WorkJoinArgs),
    /// Explicitly release a workspace declaration (idle does not release it).
    Leave(WorkLeaveArgs),
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum WorkModeArg {
    Read,
    Edit,
    Review,
}

#[derive(Args)]
pub(crate) struct WorkJoinArgs {
    /// Directory to participate in; does not relocate the session or change its identity.
    pub(crate) workspace: PathBuf,
    #[arg(long)]
    pub(crate) task: String,
    #[arg(long, value_enum, default_value = "edit")]
    pub(crate) mode: WorkModeArg,
    /// Relative file/directory scope or glob; repeat for multiple scopes.
    #[arg(long, value_name = "SCOPE")]
    pub(crate) paths: Vec<String>,
}

#[derive(Args)]
pub(crate) struct WorkLeaveArgs {
    pub(crate) workspace: PathBuf,
}
