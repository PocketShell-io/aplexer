use super::cli_examples::*;
use super::completions::{engine_completions, session_tag_completions};
use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Args)]
pub(crate) struct MessageArgs {
    #[command(subcommand)]
    pub(crate) command: MessageCommand,
}

#[derive(Subcommand)]
pub(crate) enum MessageCommand {
    /// Send a message to a tag, a broadcast, or an engine filter.
    #[command(after_help = MESSAGE_SEND_EXAMPLES)]
    Send(MessageSendArgs),
    /// Reply to a received message (threads via reply_to).
    #[command(after_help = MESSAGE_REPLY_EXAMPLES)]
    Reply(MessageReplyArgs),
    /// Submit an existing inbox message after inspecting a fresh, empty prompt.
    Deliver(MessageDeliverArgs),
    /// List unread messages addressed to the calling session.
    #[command(after_help = MESSAGE_INBOX_EXAMPLES)]
    Inbox(MessageInboxArgs),
    /// Wait for unread messages addressed to the calling session.
    Wait(MessageWaitArgs),
    /// Show the whole workspace conversation, in id (time) order.
    #[command(after_help = MESSAGE_LOG_EXAMPLES)]
    Log(MessageLogArgs),
    /// Show one message by id.
    #[command(after_help = MESSAGE_SHOW_EXAMPLES)]
    Show(MessageShowArgs),
    /// Acknowledge messages so they stop appearing in `inbox`.
    #[command(after_help = MESSAGE_ACK_EXAMPLES)]
    Ack(MessageAckArgs),
    /// Prune expired/over-cap messages from a workspace mailbox.
    #[command(after_help = MESSAGE_GC_EXAMPLES)]
    Gc(MessageGcArgs),
    /// Internal PostToolUse mailbox notice callback.
    #[command(hide = true)]
    HookNotice(MessageHookNoticeArgs),
}

#[derive(Args)]
pub(crate) struct MessageDeliverArgs {
    /// Original durable message ID; delivery never creates another envelope.
    #[arg(value_name = "MESSAGE_ID")]
    pub(crate) message_id: Uuid,
    /// Destination mailbox (defaults to the calling session workspace).
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum NoticeEngine {
    Claude,
    Codex,
}

#[derive(Args)]
pub(crate) struct MessageHookNoticeArgs {
    #[arg(long, value_enum)]
    pub(crate) engine: NoticeEngine,
}

/// Flags shared by `send` and `reply` for choosing/framing pane delivery
/// (design doc section 6.2).
#[derive(Args)]
pub(crate) struct PaneDeliveryArgs {
    #[arg(
        long,
        help = "Persist the message, then also inject it as terminal input into the target's PTY"
    )]
    pub(crate) pane: bool,
    #[arg(
        long = "or-inbox",
        help = "Return success with the durable inbox copy if pane injection fails (otherwise report an error with its stored message id)"
    )]
    pub(crate) or_inbox: bool,
    #[arg(
        long,
        help = "With --pane: suppress the '[aplexer message from ...]' frame"
    )]
    pub(crate) raw: bool,
    #[arg(
        long = "no-enter",
        help = "With --pane: do not append a trailing return. Enter is sent by default (the tmuxctl behavior) so an injected message actually submits"
    )]
    pub(crate) no_enter: bool,
}

#[derive(Args)]
pub(crate) struct MessageSendArgs {
    #[arg(
        long,
        value_name = "PATH",
        help = "Destination workspace for a targeted message"
    )]
    pub(crate) workspace: Option<PathBuf>,
    #[arg(
        long,
        value_name = "TAG",
        add = session_tag_completions(),
        help = "Send to one session, addressed by tag"
    )]
    pub(crate) to: Option<String>,
    #[arg(long, help = "Broadcast to every other session in the workspace")]
    pub(crate) all: bool,
    #[arg(
        long = "to-engine",
        value_name = "ENGINE",
        add = engine_completions(),
        help = "Broadcast to sessions of one engine"
    )]
    pub(crate) to_engine: Option<String>,
    #[arg(
        long,
        help = "Allow sending to a tag that has never existed in this workspace"
    )]
    pub(crate) queue: bool,
    #[arg(
        long,
        default_value = "note",
        help = "note (default) | handoff | reply | any string"
    )]
    pub(crate) kind: String,
    #[arg(long, value_name = "JSON", help = "Opaque structured payload")]
    pub(crate) data: Option<String>,
    #[command(flatten)]
    pub(crate) pane_delivery: PaneDeliveryArgs,
    #[arg(
        long,
        value_name = "TAG",
        help = "Sender identity override (default: APLEXER_TAG or anonymous)"
    )]
    pub(crate) from: Option<String>,
    /// The message body
    #[arg(value_name = "TEXT")]
    pub(crate) text: String,
}

#[derive(Args)]
pub(crate) struct MessageReplyArgs {
    /// Id of the message being replied to (from `a message inbox`)
    #[arg(value_name = "MESSAGE_ID")]
    pub(crate) message_id: Uuid,
    #[command(flatten)]
    pub(crate) pane_delivery: PaneDeliveryArgs,
    /// Sender identity override (default: APLEXER_TAG or anonymous)
    #[arg(long, value_name = "TAG")]
    pub(crate) from: Option<String>,
    /// Opaque structured payload
    #[arg(long, value_name = "JSON")]
    pub(crate) data: Option<String>,
    #[arg(long, value_name = "KIND", help = "Defaults to \"reply\"")]
    pub(crate) kind: Option<String>,
    /// The reply body
    #[arg(value_name = "TEXT")]
    pub(crate) text: String,
}

#[derive(Args)]
pub(crate) struct MessageInboxArgs {
    #[arg(
        long,
        help = "Unread messages only (this is also the default with no flag)"
    )]
    pub(crate) new: bool,
    #[arg(
        long,
        value_name = "TAG",
        help = "Consumer identity override (default: APLEXER_SESSION_ID)"
    )]
    pub(crate) from: Option<String>,
}

#[derive(Args)]
pub(crate) struct MessageWaitArgs {
    /// Maximum seconds to wait; zero checks immediately. Does not acknowledge messages.
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    pub(crate) timeout: u64,
}

#[derive(Args)]
pub(crate) struct MessageLogArgs {
    /// Workspace whose conversation to show (default: the current workspace)
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct MessageShowArgs {
    /// Id of the message to show
    #[arg(value_name = "MESSAGE_ID")]
    pub(crate) message_id: Uuid,
}

#[derive(Args)]
pub(crate) struct MessageAckArgs {
    /// Ids to acknowledge (from `a message inbox`)
    #[arg(value_name = "MESSAGE_ID")]
    pub(crate) message_ids: Vec<Uuid>,
    #[arg(
        long,
        help = "Ack every currently-unread message addressed to this consumer"
    )]
    pub(crate) all: bool,
    /// Consumer identity override (default: APLEXER_SESSION_ID)
    #[arg(long, value_name = "TAG")]
    pub(crate) from: Option<String>,
}

#[derive(Args)]
pub(crate) struct MessageGcArgs {
    /// Workspace whose mailbox to prune (default: the current workspace)
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
}
