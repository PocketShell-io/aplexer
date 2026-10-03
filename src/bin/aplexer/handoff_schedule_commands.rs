use super::*;
use aplexer::handoff_schedule::{
    claim_slot, evaluate, fired_slots, load_schedule, local_wall, next_due, parse_hhmm,
    remove_handoff_state, save_schedule, schedule_path, slot_fired, FireDecision, HandoffSchedule,
    HandoffTaskSpec, HANDOFF_SCHEDULE_SCHEMA_VERSION,
};

/// `a task handoff enable|disable|status|fire` — the handoff plugin's CLI
/// wrapper over `a task run`. Everything heavy is delegated: `fire` launches
/// through `cmd_task` itself, so engine/profile resolution, evidence
/// records, notices, exit codes and cutoff routing are literally the same
/// code paths. This file only owns the schedule state and the due decision.
pub(crate) fn cmd_task_handoff(
    paths: &Paths,
    args: TaskHandoffCommand,
    json_output: bool,
) -> Result<()> {
    match args {
        TaskHandoffCommand::Enable(args) => enable_handoff(paths, *args, json_output),
        TaskHandoffCommand::Disable(args) => disable_handoff(paths, args, json_output),
        TaskHandoffCommand::Status(args) => status_handoff(paths, args, json_output),
        TaskHandoffCommand::Fire(args) => fire_handoff(paths, args, json_output),
    }
}

fn enable_handoff(paths: &Paths, args: TaskHandoffEnableArgs, json_output: bool) -> Result<()> {
    // Evidence stays per-launch: pinning one output directory would make the
    // second fire collide with the first fire's RESULT.json.
    if args.task.output_dir.is_some() || args.task.overwrite {
        bail!(
            "--output-dir/--overwrite are per-launch concerns and are not scheduled; \
             every fire writes its own .aplexer-tasks evidence directory"
        );
    }
    let at = args.at.as_deref().map(parse_hhmm).transpose()?;
    // Validate now, at enable time — never on some future fire.
    if args.task.cutoff.is_some() != args.task.cutoff_engine.is_some() {
        bail!("--cutoff requires --cutoff-engine and vice versa");
    }
    if let Some(cutoff) = &args.task.cutoff {
        aplexer::task::parse_offset_timestamp(cutoff)?;
    }
    parse_env(&args.task.env)?;
    fs::File::open(&args.task.prompt_file).with_context(|| {
        format!(
            "prompt file {} must be readable",
            args.task.prompt_file.display()
        )
    })?;

    let schedule = HandoffSchedule {
        schema_version: HANDOFF_SCHEDULE_SCHEMA_VERSION,
        created_at: aplexer::task::rfc3339_utc(now_ms()),
        at: args.at.clone(),
        task: task_args_to_spec(&args.task),
    };
    let path = save_schedule(&paths.state_root, &schedule)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "enabled": true,
                "schedule_path": path,
                "schedule": schedule,
                "next_due_secs": next_due(at, now_ms())?,
            }))?
        );
    } else {
        println!("handoff schedule enabled: {}", path.display());
        print_schedule_summary(&schedule, at)?;
        println!(
            "no timer is installed by aplexer; call `a task handoff fire` from your own \
             scheduler (plugins/handoff/INSTALL.md has cron and systemd examples)"
        );
    }
    Ok(())
}

fn disable_handoff(paths: &Paths, _args: TaskHandoffDisableArgs, json_output: bool) -> Result<()> {
    let path = schedule_path(&paths.state_root);
    let removed = remove_handoff_state(&paths.state_root)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "removed": removed,
                "schedule_path": path,
            }))?
        );
    } else if removed {
        println!("handoff schedule removed: {}", path.display());
        println!("running tasks were not touched; launches after this point use plain `a task run` semantics");
    } else {
        println!(
            "handoff already disabled (no schedule at {}); nothing to remove",
            path.display()
        );
    }
    Ok(())
}

fn status_handoff(paths: &Paths, _args: TaskHandoffStatusArgs, json_output: bool) -> Result<()> {
    let schedule = load_schedule(&paths.state_root)?;
    let fired = fired_slots(&paths.state_root)?;
    let at = schedule
        .as_ref()
        .and_then(|schedule| schedule.at.as_deref())
        .map(parse_hhmm)
        .transpose()?;
    let next_due_secs = match (&schedule, at) {
        (Some(_), Some(at)) => Some(next_due(Some(at), now_ms())?),
        _ => None,
    };
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "enabled": schedule.is_some(),
                "schedule_path": schedule_path(&paths.state_root),
                "schedule": schedule,
                "fired": fired,
                "next_due_secs": next_due_secs,
            }))?
        );
        return Ok(());
    }
    match schedule {
        None => {
            println!(
                "handoff: disabled (no schedule at {})",
                schedule_path(&paths.state_root).display()
            );
            println!("enable with: a task handoff enable --prompt-file FILE [--at HH:MM] …");
        }
        Some(schedule) => {
            println!(
                "handoff: enabled — {}",
                schedule_path(&paths.state_root).display()
            );
            print_schedule_summary(&schedule, at)?;
        }
    }
    if let Some(secs) = next_due_secs {
        println!(
            "next due: {}",
            aplexer::task::rfc3339_utc(secs as u64 * 1000)
        );
    }
    if fired.is_empty() {
        println!("fired slots: none");
    } else {
        println!("fired slots: {}", fired.join(", "));
    }
    Ok(())
}

fn fire_handoff(paths: &Paths, _args: TaskHandoffFireArgs, json_output: bool) -> Result<()> {
    let Some(schedule) = load_schedule(&paths.state_root)? else {
        // A schedule-less fire is a successful no-op: removing the schedule
        // must never break an existing timer entry, and a fresh install
        // fires nothing until enable is run.
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "fired": false,
                    "reason": "disabled",
                }))?
            );
        } else {
            println!(
                "handoff: disabled (no schedule at {}); nothing to fire",
                schedule_path(&paths.state_root).display()
            );
        }
        return Ok(());
    };
    let now = now_ms();
    let at = schedule.at.as_deref().map(parse_hhmm).transpose()?;
    let decision = evaluate(local_wall(i64::try_from(now / 1000)?)?, at, now, |slot| {
        slot_fired(&paths.state_root, slot)
    })?;
    let slot = match decision {
        FireDecision::Due { slot } => slot,
        FireDecision::NotDue { next_due_secs } => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "fired": false, "reason": "not-due" }))?
                );
            } else {
                println!(
                    "handoff: not due (next due {})",
                    aplexer::task::rfc3339_utc(next_due_secs as u64 * 1000)
                );
            }
            return Ok(());
        }
        FireDecision::AlreadyFired { slot } => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "fired": false, "reason": "already-fired", "slot": slot })
                    )?
                );
            } else {
                println!("handoff: already fired for slot {slot}; nothing to do");
            }
            return Ok(());
        }
    };
    // The claim is the authority: a racing fire that loses here launches
    // nothing, keeping at-most-one-launch-per-slot exact.
    if !claim_slot(&paths.state_root, &slot, now)? {
        println!("handoff: slot {slot} claimed by a concurrent fire; nothing to do");
        return Ok(());
    }
    if !json_output {
        println!("handoff: due (slot {slot}); launching scheduled task");
    }
    let task_args = spec_to_task_args(schedule.task);
    // In-process reuse of `a task run`: same resolution, records, notice,
    // exit code (this call ends the process with the task's real status).
    task_commands::cmd_task(paths, task_args, json_output)
}

fn print_schedule_summary(schedule: &HandoffSchedule, at: Option<(u32, u32)>) -> Result<()> {
    let task = &schedule.task;
    match at {
        Some((hh, mm)) => println!("launches: daily at {hh:02}:{mm:02} local (machine timezone)"),
        None => println!("launches: whenever `fire` runs (at most once per local minute)"),
    }
    println!(
        "task: prompt {}, engine {}, cwd {}",
        task.prompt_file.display(),
        task.engine.as_deref().unwrap_or("<configured default>"),
        task.cwd
            .as_deref()
            .map(|cwd| cwd.display().to_string())
            .unwrap_or_else(|| "<fire's current directory>".into())
    );
    if let (Some(cutoff), Some(cutoff_engine)) = (&task.cutoff, &task.cutoff_engine) {
        println!("cutoff routing: at/after {cutoff} new launches use {cutoff_engine}");
    }
    Ok(())
}

/// The saved spec, replayed as `a task run` arguments at fire time. Evidence
/// fields stay per-launch (`output_dir: None`, `overwrite: false` — enable
/// refuses to schedule them).
fn spec_to_task_args(spec: HandoffTaskSpec) -> TaskRunArgs {
    TaskRunArgs {
        prompt_file: spec.prompt_file,
        engine: spec.engine,
        profile: spec.profile,
        cwd: spec.cwd,
        output_dir: None,
        timeout_secs: spec.timeout_secs,
        engine_args: spec.engine_args,
        env: spec.env,
        no_skip_permissions: spec.no_skip_permissions,
        notify_to: spec.notify_to,
        notify_workspace: spec.notify_workspace,
        no_notify: spec.no_notify,
        cutoff: spec.cutoff,
        cutoff_engine: spec.cutoff_engine,
        overwrite: false,
    }
}

fn task_args_to_spec(args: &TaskRunArgs) -> HandoffTaskSpec {
    HandoffTaskSpec {
        prompt_file: args.prompt_file.clone(),
        engine: args.engine.clone(),
        profile: args.profile.clone(),
        cwd: args.cwd.clone(),
        timeout_secs: args.timeout_secs,
        engine_args: args.engine_args.clone(),
        env: args.env.clone(),
        no_skip_permissions: args.no_skip_permissions,
        notify_to: args.notify_to.clone(),
        notify_workspace: args.notify_workspace.clone(),
        no_notify: args.no_notify,
        cutoff: args.cutoff.clone(),
        cutoff_engine: args.cutoff_engine.clone(),
    }
}
