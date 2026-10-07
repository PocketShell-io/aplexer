//! `a wake set|list|off`: the CLI for per-session self-wakeup jobs
//! (`aplexer::wake`). Defaults to the session the command runs inside, so an
//! agent registers and cancels its own wake-ups with no arguments.

use super::*;
use aplexer::wake::{format_duration_ms, parse_duration_ms, WakeJob, WakeMode};
use clap::{Args, Subcommand};

pub(crate) const WAKE_EXAMPLES: &str = r#"Examples:
  a wake set --every 2m                    keep pinging this session every 2m while it is idle, until `a wake off`
  a wake set --every 2m --until-message    same, but stop once a message arrives / is acked
  a wake set --once --in 5m                one wake-up in 5m, then the job removes itself
  a wake set --every 5m --text "check CI"  custom one-line wake prompt
  a wake list                              jobs for this session (--all: every session)
  a wake off                               turn it off (idempotent)
"#;

#[derive(Args)]
pub(crate) struct WakeArgs {
    #[command(subcommand)]
    pub(crate) command: WakeCommand,
}

#[derive(Subcommand)]
pub(crate) enum WakeCommand {
    /// Register (replace) this session's self-wakeup job.
    Set(WakeSetArgs),
    /// Show wake jobs.
    List(WakeListArgs),
    /// Turn the wake-up off.
    Off(WakeOffArgs),
}

#[derive(Args)]
pub(crate) struct WakeSetArgs {
    /// Constant mode: wake every INTERVAL (e.g. 90s, 2m, 1h) until turned off.
    #[arg(long, value_name = "INTERVAL", conflicts_with_all = ["once", "delay"], required_unless_present = "once")]
    pub(crate) every: Option<String>,
    /// One-time mode: wake once after `--in`, then remove the job.
    #[arg(long, requires = "delay")]
    pub(crate) once: bool,
    /// Delay before the one-time wake (with --once).
    #[arg(long = "in", value_name = "DELAY", id = "delay")]
    pub(crate) delay: Option<String>,
    /// Wake prompt typed into the session (one line, default is a generic nudge).
    #[arg(long, value_name = "TEXT")]
    pub(crate) text: Option<String>,
    /// Opt in: stop automatically once a message addressed to this session
    /// arrives or is acked. Without it a constant job keeps pinging.
    #[arg(long)]
    pub(crate) until_message: bool,
    /// Safety cap: the job removes itself after this long (default 6h).
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_lifetime: Option<String>,
    /// Session to target (default: the session this command runs inside).
    #[arg(long, value_name = "SESSION", add = session_selector_completions())]
    pub(crate) session: Option<String>,
}

#[derive(Args)]
pub(crate) struct WakeListArgs {
    /// List every session's job, not just this one's.
    #[arg(long)]
    pub(crate) all: bool,
    /// Session to show (default: the session this command runs inside).
    #[arg(long, value_name = "SESSION", add = session_selector_completions())]
    pub(crate) session: Option<String>,
}

#[derive(Args)]
pub(crate) struct WakeOffArgs {
    /// Session to target (default: the session this command runs inside).
    #[arg(long, value_name = "SESSION", add = session_selector_completions())]
    pub(crate) session: Option<String>,
    /// Turn off the wake-up on every live session.
    #[arg(long, conflicts_with = "session")]
    pub(crate) all: bool,
}

fn target(paths: &Paths, session: Option<&str>) -> Result<SessionRecord> {
    match session {
        Some(selector) => resolve_record(paths, Some(selector), None, None),
        None => {
            let id = discover_session_id().ok_or_else(|| {
                anyhow!(
                    "not inside an aplexer session (APLEXER_SESSION_ID not set); pass --session"
                )
            })?;
            read_record(&paths.record(id))
                .with_context(|| format!("session {id} has no persisted record"))
        }
    }
}

pub(crate) fn describe(job: &WakeJob, now: u64) -> String {
    let mode = match job.mode {
        WakeMode::Once => format!(
            "once in {}",
            format_duration_ms(job.next_due_ms.saturating_sub(now))
        ),
        WakeMode::Every => format!(
            "every {}, next {}",
            format_duration_ms(job.interval_ms),
            format_duration_ms(job.next_due_ms.saturating_sub(now))
        ),
    };
    format!(
        "{mode}; fired {}x; expires in {}{}",
        job.fires,
        format_duration_ms(job.expires_at_ms.saturating_sub(now)),
        if job.until_message {
            "; until-message"
        } else {
            ""
        }
    )
}

pub(crate) fn cmd_wake(paths: &Paths, args: WakeArgs, json_output: bool) -> Result<()> {
    match args.command {
        WakeCommand::Set(a) => {
            let record = target(paths, a.session.as_deref())?;
            let (mode, ms) = if a.once {
                (
                    WakeMode::Once,
                    parse_duration_ms(a.delay.as_deref().unwrap_or_default())?,
                )
            } else {
                (
                    WakeMode::Every,
                    parse_duration_ms(a.every.as_deref().unwrap_or_default())?,
                )
            };
            let life = a
                .max_lifetime
                .as_deref()
                .map(parse_duration_ms)
                .transpose()?;
            let job = WakeJob::new(mode, ms, a.text, a.until_message, life, now_ms())?;
            let updated: SessionRecord =
                serde_json::from_value(rpc_simple(&record, Operation::WakeSet { job }, None)?)?;
            show(&updated, json_output)
        }
        WakeCommand::List(a) => {
            let records = if a.all {
                list_records(paths)?
            } else {
                vec![target(paths, a.session.as_deref())?]
            };
            let now = now_ms();
            if json_output {
                let rows: Vec<_> = records
                    .iter()
                    .filter(|r| r.wake.is_some())
                    .map(|r| json!({"id": r.id, "selector": r.selector(), "wake": r.wake}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                let mut any = false;
                for r in records.iter().filter(|r| r.wake.is_some()) {
                    any = true;
                    println!(
                        "{}: {}",
                        r.selector(),
                        describe(r.wake.as_ref().unwrap(), now)
                    );
                }
                if !any {
                    println!("no wake jobs");
                }
            }
            Ok(())
        }
        WakeCommand::Off(a) => {
            let records = if a.all {
                list_records(paths)?
                    .into_iter()
                    .filter(|r| r.wake.is_some() && r.worker_alive())
                    .collect()
            } else {
                vec![target(paths, a.session.as_deref())?]
            };
            for record in &records {
                rpc_simple(record, Operation::WakeOff, None)?;
                if !json_output {
                    println!("{}: wake off", record.selector());
                }
            }
            if json_output {
                println!(
                    "{}",
                    json!({"off": records.iter().map(|r| r.id).collect::<Vec<_>>()})
                );
            }
            Ok(())
        }
    }
}

fn show(record: &SessionRecord, json_output: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"id": record.id, "wake": record.wake}))?
        );
    } else if let Some(job) = &record.wake {
        println!("{}: wake {}", record.selector(), describe(job, now_ms()));
    }
    Ok(())
}
