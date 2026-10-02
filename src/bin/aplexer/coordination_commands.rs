use super::*;
use aplexer::coordination::{self, WorkMode};

pub(crate) fn cmd_context(paths: &Paths, args: ContextArgs, json_output: bool) -> Result<()> {
    if let Some(ContextCommand::Hook(hook)) = args.command {
        // Optional integration must never prevent an ordinary agent tool from running.
        if let Ok(Some(output)) =
            aplexer::awareness::hook_context(paths, hook.engine.as_str(), io::stdin().lock())
        {
            println!("{output}");
        }
        return Ok(());
    }
    let workspace = canonical_workspace(args.workspace.as_deref().unwrap_or(Path::new(".")))?;
    let session_id = discover_session_id().filter(|id| read_record(&paths.record(*id)).is_ok());
    let context = coordination::context(paths, session_id, &workspace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&context)?);
    } else {
        println!("{}", coordination::render_context(&context));
    }
    Ok(())
}

pub(crate) fn cmd_work(paths: &Paths, args: WorkArgs, json_output: bool) -> Result<()> {
    let session_id = discover_session_id()
        .ok_or_else(|| anyhow!("work declarations require an aplexer session identity"))?;
    match args.command {
        WorkCommand::Join(join) => {
            let mode = match join.mode {
                WorkModeArg::Read => WorkMode::Read,
                WorkModeArg::Edit => WorkMode::Edit,
                WorkModeArg::Review => WorkMode::Review,
            };
            let participation = coordination::join(
                paths,
                session_id,
                &join.workspace,
                &join.task,
                mode,
                &join.paths,
            )?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&participation)?);
            } else {
                println!("Declared work in {}. Check `a context` and coordinate overlapping scopes through `a message`.", join.workspace.display());
            }
        }
        WorkCommand::Leave(leave) => {
            let released = coordination::leave(paths, session_id, &leave.workspace)?;
            if json_output {
                println!("{}", json!({"released": released}));
            } else {
                println!(
                    "{}",
                    if released {
                        "Released workspace declaration."
                    } else {
                        "No active declaration for this workspace."
                    }
                );
            }
        }
    }
    Ok(())
}
