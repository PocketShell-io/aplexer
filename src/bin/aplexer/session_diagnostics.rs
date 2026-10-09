use super::*;

/// `a whoami` -- lets an agent or script running INSIDE a session (or a
/// human at its prompt) ask "am I in an aplexer session, and if so which
/// one" without hand-parsing environment variables. Every workload already
/// has APLEXER_SESSION_ID/WORKSPACE/TAG injected (see spawn_workload in
/// worker.rs) -- this just resolves the id against the session's persisted
/// record for the fuller picture (engine, profile, phase) and gives a
/// stable, scriptable "nothing/non-zero if not inside one" contract, the
/// same shape `$TMUX` serves for tmux but structured instead of a bare path.
pub(crate) fn cmd_whoami(paths: &Paths, json_output: bool) -> Result<()> {
    let Some(id) = discover_session_id() else {
        // Deliberately silent on stdout either way -- a script doing
        // `id=$(a whoami --json)` should see empty output and rely on the
        // exit code, not have to filter out a "not in a session" sentence.
        if !json_output {
            eprintln!("not inside an aplexer session");
        }
        std::process::exit(1);
    };
    let record = read_record(&paths.record(id)).with_context(|| {
        format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
    })?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&public_session_record(&record))?
        );
    } else {
        println!("id: {}", record.id);
        println!("selector: {}", record.selector());
        println!("engine: {}", record.engine);
        if let Some(profile) = &record.profile {
            println!("profile: {profile}");
        }
        // On a terminal, lead with the honest semantic state and show the
        // underlying lifecycle when it differs; redirected output keeps the
        // raw phase word the pre-UX build printed.
        if io::stdout().is_terminal() {
            let (state, source) = session_ui_state(&record, now_ms());
            println!("state: {state}{}", state_source_suffix(source));
            let lifecycle =
                derived_liveness(&record.phase, record.worker_alive(), record.created_at_ms);
            if lifecycle != state {
                println!("lifecycle: {lifecycle}");
            }
        } else {
            println!("state: {}", record.phase.name());
        }
    }
    Ok(())
}

/// `a state-report <idle|waiting|working>`
/// (docs/pocketshell-integration-plan.md Open question #2, "Agent-state
/// ingestion"): lets a hook running INSIDE a session push its own semantic
/// state -- the missing half of `a watch --jsonl`'s `agent.state` event,
/// which otherwise only has a coarse PTY-recency heuristic to go on (see
/// watch.rs's `fresh_reported_state`/`derive_agent_state_with_source` for
/// exactly how a push is merged with that heuristic and for how long it
/// stays authoritative).
///
/// Resolves its target exactly like `a whoami` -- via the injected
/// `APLEXER_SESSION_ID`, never a selector -- because a hook script has no
/// notion of "which session" other than the one it is running inside; see
/// `cmd_whoami`'s doc comment for the shared mechanism (`discover_session_id`,
/// the same env var `worker.rs::spawn_workload` injects into every
/// session). Same exit-code contract as `a whoami`: a plain `exit(1)` with
/// one stderr line when `APLEXER_SESSION_ID` is unset (so a hook wired as
/// `a state-report waiting || true` degrades silently outside aplexer);
/// any other failure (record missing, worker dead/unreachable, invalid
/// state rejected by the worker) propagates through `?` to `main`'s
/// generic `a: {error}` / exit(1) handler, same as every other subcommand.
///
/// What this repo does NOT do here (deliberately): install the hooks that
/// call this command. That wiring lives in `a init` (`aplexer::hooks`),
/// which merges a `state-report` hook into every configured engine
/// (Claude Stop/Notification, Codex hooks/notify, OpenCode plugin, Grok
/// and Gemini hooks) — this command is the ingestion primitive it builds
/// on. `a init --check --json` is the machine-readable way to verify the
/// wiring is present.
pub(crate) fn cmd_state_report(paths: &Paths, state: ReportedState) -> Result<()> {
    // Stamp BEFORE any I/O (review round 4 on 2ab0860): the engine invoked
    // this process at the event, so process start is the closest
    // client-side proxy for the engine event time. Stamping after the
    // stdin read/parse let a slow or queued invocation acquire a later
    // stamp than a newer prompt's report and slip past the worker's
    // ordering fence. Residual race: engine-side delay before the hook
    // process starts is unobservable from here -- the payload schema
    // (verified against the installed engine binary) carries no engine
    // timestamp.
    let event_ms = now_ms();
    let Some(id) = discover_session_id() else {
        eprintln!("a state-report: not inside an aplexer session (APLEXER_SESSION_ID not set)");
        std::process::exit(1);
    };
    let record = read_record(&paths.record(id)).with_context(|| {
        format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
    })?;
    // Claude pipes its hook payload (session_id, hook_event_name,
    // background_tasks, ...) to every hook on stdin. The gated Stop
    // decision consumes the whole payload; the plain working/waiting hooks
    // read it only to pick up the engine session -- and only when stdin is
    // actually a hook's pipe, never a terminal, so a manual
    // `a state-report working` behaves exactly as before. Other engines'
    // payload shapes are not verified here; they never send gated-idle and
    // report without a session name, which bypasses the worker's session
    // fence (the pre-fence behavior).
    let hook_payload: Option<serde_json::Value> = if matches!(state, ReportedState::GatedIdle)
        || (record.engine == "claude" && !io::stdin().is_terminal())
    {
        let mut payload = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut payload);
        serde_json::from_str(&payload).ok()
    } else {
        None
    };
    let engine_session_id = hook_payload.as_ref().and_then(engine_session_id_of);
    // Claude's gated Stop mode: the decision comes from the engine's own
    // hook payload on stdin -- the event name, the engine session, the
    // parent-scoped `background_tasks` registry and `stop_hook_active` --
    // never from PTY silence or a sender's claim. The wiring only exists
    // for claude (see `hooks::CLAUDE_EVENTS`); any other record engine is
    // a miswiring and refuses (the hook's `|| true` degrades it to a
    // no-op). Every evidence failure is FAIL-CLOSED: a payload that is not
    // a genuine Stop event of this engine session, a missing or malformed
    // registry, or an unparseable payload reports NOTHING -- no idle
    // without positive engine evidence (review rounds 2 and 3 on 9cd07b0).
    let state = if matches!(state, ReportedState::GatedIdle) {
        if record.engine != "claude" {
            eprintln!("a state-report: gated-idle is wired for engine claude only");
            std::process::exit(1);
        }
        let parsed = hook_payload.unwrap_or(serde_json::Value::Null);
        match gated_stop_decision(&parsed) {
            GatedStop::Working => ReportedState::Working,
            GatedStop::Idle => ReportedState::Idle,
            GatedStop::NoEvidence => {
                eprintln!(
                    "a state-report: gated Stop has no usable engine evidence \
                     (event, session, or background_tasks); reporting nothing \
                     (fail closed)"
                );
                return Ok(());
            }
        }
    } else {
        state
    };
    // Stamp the engine-event time: the hook runs this CLI synchronously at
    // the event, so client-start time IS the event time (plus CLI
    // startup). The worker fences on it -- see
    // `WorkerRuntime::report_state`.
    rpc_simple(
        &record,
        Operation::ReportState {
            state: state.as_str().to_string(),
            event_ms: Some(event_ms),
            engine_session_id,
        },
        None,
    )?;
    Ok(())
}

/// The gated Claude Stop verdict, decided entirely from the engine's own
/// hook payload (installed CLI 2.1.289 schema, read from the binary's
/// embedded definitions): `background_tasks` is "In-flight background work
/// (running/pending + backgrounded) registered in this session. Lets hooks
/// distinguish \"session is done\" from \"session is paused waiting for
/// background work to wake it\". Empty array when nothing is in flight."
///
/// - any live `background_tasks` entry -> `Working`: the foreground turn
///   ended, but the registry says the session is paused for background
///   work. Quiet children included -- PTY silence proves nothing; the
///   engine's own registry is the evidence.
/// - `stop_hook_active` -> `Working`: a Stop hook continued this turn; the
///   session is not done.
/// - an empty array of objects -> `Idle`: the engine's own done-evidence --
///   the true final boundary, whether the turn was plain or background
///   work drained and woke the session.
/// - `session_crons` never block the boundary: a scheduled wake is future
///   work whose own lifecycle reports will push `working` when it fires.
/// - the payload must name the Stop event (`hook_event_name`) and the
///   engine session it belongs to (`session_id`, a non-empty string):
///   anything else -- a miswired or foreign invocation such as the
///   deliberately unwired `SubagentStop` -- reports NOTHING, even when a
///   registry looks empty (review round 3 on 9cd07b0).
/// - field absent (older CLI), not an array, an array with a non-object
///   entry, or an unparseable payload -> `NoEvidence`: FAIL CLOSED. The
///   caller reports nothing rather than permitting an idle the engine did
///   not positively evidence (review round 2 on 9cd07b0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatedStop {
    Idle,
    Working,
    NoEvidence,
}

/// The engine session a hook payload names, if it validly names one: a
/// non-empty string `session_id` (Claude's envelope carries one on every
/// hook). Absent, mistyped, or empty means no evidence.
fn engine_session_id_of(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn gated_stop_decision(payload: &serde_json::Value) -> GatedStop {
    if payload.get("hook_event_name").and_then(|v| v.as_str()) != Some("Stop")
        || engine_session_id_of(payload).is_none()
    {
        return GatedStop::NoEvidence;
    }
    if payload.get("stop_hook_active").and_then(|v| v.as_bool()) == Some(true) {
        return GatedStop::Working;
    }
    match payload.get("background_tasks") {
        Some(serde_json::Value::Array(tasks)) => {
            if tasks.iter().any(|task| !task.is_object()) {
                return GatedStop::NoEvidence;
            }
            if tasks.is_empty() {
                GatedStop::Idle
            } else {
                GatedStop::Working
            }
        }
        _ => GatedStop::NoEvidence,
    }
}

#[cfg(test)]
mod gated_stop_tests {
    use super::*;

    #[test]
    fn empty_registry_is_the_engine_done_evidence() {
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"hook_event_name":"Stop","session_id":"s1","background_tasks":[]}"#,
        )
        .unwrap();
        assert_eq!(gated_stop_decision(&payload), GatedStop::Idle);
    }

    #[test]
    fn any_live_background_task_says_working_even_a_quiet_one() {
        let one: serde_json::Value = serde_json::from_str(
            r#"{"hook_event_name":"Stop","session_id":"s1",
                "background_tasks":[{"task_id":"t1","state":"running"}]}"#,
        )
        .unwrap();
        assert_eq!(gated_stop_decision(&one), GatedStop::Working);
        let several: serde_json::Value = serde_json::from_str(
            r#"{"hook_event_name":"Stop","session_id":"s1",
                "background_tasks":[{"task_id":"a"},{"task_id":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(gated_stop_decision(&several), GatedStop::Working);
    }

    #[test]
    fn a_stop_hook_continuation_is_not_done() {
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"hook_event_name":"Stop","session_id":"s1",
                "background_tasks":[],"stop_hook_active":true}"#,
        )
        .unwrap();
        assert_eq!(gated_stop_decision(&payload), GatedStop::Working);
    }

    #[test]
    fn session_crons_do_not_block_the_done_boundary() {
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"hook_event_name":"Stop","session_id":"s1",
                "background_tasks":[],"session_crons":[{"id":"c1"}]}"#,
        )
        .unwrap();
        assert_eq!(gated_stop_decision(&payload), GatedStop::Idle);
    }

    #[test]
    fn missing_evidence_fails_closed_instead_of_permitting_idle() {
        // Field absent entirely (older CLI schema).
        let absent = serde_json::json!({"hook_event_name":"Stop","session_id":"s1"});
        assert_eq!(gated_stop_decision(&absent), GatedStop::NoEvidence);
        // An unparseable payload parses as Null upstream.
        assert_eq!(
            gated_stop_decision(&serde_json::Value::Null),
            GatedStop::NoEvidence
        );
        // Registry present but not an array.
        let not_array = serde_json::json!({"background_tasks":"none"});
        assert_eq!(gated_stop_decision(&not_array), GatedStop::NoEvidence);
        // Array with a schema-violating (non-object) entry: malformed.
        let malformed = serde_json::json!({"background_tasks":[null]});
        assert_eq!(gated_stop_decision(&malformed), GatedStop::NoEvidence);
    }

    #[test]
    fn a_non_stop_event_or_an_unnamed_session_reports_nothing() {
        // SubagentStop stays deliberately unwired (hooks/mod.rs): its
        // shape must not idle even with an empty registry.
        let subagent = serde_json::json!(
            {"hook_event_name":"SubagentStop","session_id":"s1","background_tasks":[]}
        );
        assert_eq!(gated_stop_decision(&subagent), GatedStop::NoEvidence);
        // No event name at all.
        let unnamed_event = serde_json::json!({"session_id":"s1","background_tasks":[]});
        assert_eq!(gated_stop_decision(&unnamed_event), GatedStop::NoEvidence);
        // No engine session named (older envelope).
        let no_session = serde_json::json!({"hook_event_name":"Stop","background_tasks":[]});
        assert_eq!(gated_stop_decision(&no_session), GatedStop::NoEvidence);
        // Empty or mistyped session name.
        let empty_session =
            serde_json::json!({"hook_event_name":"Stop","session_id":"","background_tasks":[]});
        assert_eq!(gated_stop_decision(&empty_session), GatedStop::NoEvidence);
        let numeric_session =
            serde_json::json!({"hook_event_name":"Stop","session_id":7,"background_tasks":[]});
        assert_eq!(gated_stop_decision(&numeric_session), GatedStop::NoEvidence);
    }
}

/// `a init [--check] [--uninstall] [--engine NAME]`
///
/// Machine-wide agent-state hook installation: merges an `a state-report`
/// hook into every agent engine aplexer knows how to launch (claude, codex
/// — which also covers the zcodex variant via shared `CODEX_HOME` — grok,
/// gemini, opencode), including each configured profile's config dir, so a
/// session reports `working`/`waiting`/`idle` instead of leaving every
/// consumer to guess from PTY-output recency. See `aplexer::hooks` for the
/// per-engine mechanisms and the merge-never-clobber rules.
///
/// It also manages the shell-prompt indicator (`aplexer::shell_prompt`): a
/// `[tag]` function for `PS1`/`PROMPT` that resolves the tag live, so `a
/// rename` reflects on the next prompt instead of showing the stale
/// `APLEXER_TAG` spawn value. Only `~/.bashrc` / `~/.zshrc` files that
/// already exist are touched (marker-bracketed block, appended once); the
/// prompt string itself is never rewritten — install prints the one-line
/// wiring instead. With `--engine`, only that engine's hooks are touched
/// and the prompt block is left alone entirely.
///
/// Modes (exactly one):
///
/// - default: install (idempotent; only writes files that change).
/// - `--check`: touch nothing; print per-engine status and exit 0 when
///   fully initialized, 1 otherwise. With `--json` this prints
///   `{"initialized": bool, "engines": [...]}` — the machine contract the
///   PocketShell host CLI automates against (run `a init --check --json`;
///   when it reports `initialized: false`, run `a init`). Both the JSON
///   and the exit code additionally cover the prompt block (missing rc
///   files count as satisfied: nothing to manage), with per-shell detail
///   in an additive `"prompt"` array.
/// - `--uninstall`: remove our hooks again.
///
/// `--engine` limits any mode to one engine (`zcodex` maps onto `codex`).
pub(crate) fn cmd_init(paths: &Paths, args: InitArgs, json_output: bool) -> Result<()> {
    if args.check && args.uninstall {
        bail!("`a init --check` and `a init --uninstall` cannot be combined");
    }
    let filter = args
        .engine
        .as_deref()
        .map(aplexer::hooks::normalize_engine_filter)
        .transpose()?;
    // Profile config dirs (CLAUDE_CONFIG_DIR / CODEX_HOME) extend the
    // install targets past the default homes, so a profile session reports
    // state just like a default one. A broken user config fails here the
    // same way it fails every other command.
    let config = Config::load(paths)?;
    let profile_envs: Vec<BTreeMap<String, String>> = config
        .profiles
        .values()
        .map(|profile| profile.env.clone())
        .collect();
    let targets = aplexer::hooks::resolve_targets_from_env(&profile_envs)?;
    let a_bin = aplexer::hooks::resolve_a_bin();
    // The prompt block is global, not per-engine: an `--engine` filter
    // scopes the whole invocation to that engine's hooks and leaves the
    // rc files alone. Without a filter the prompt rides every mode.
    let prompt_targets = match filter {
        None => Some(aplexer::shell_prompt::prompt_targets_from_env()?),
        Some(_) => None,
    };
    let prompt_ref = prompt_targets.as_deref().unwrap_or(&[]);

    if args.check {
        let statuses = aplexer::hooks::check(&targets, filter);
        let prompt = aplexer::shell_prompt::check(prompt_ref);
        let initialized = statuses.iter().all(|status| status.installed)
            && prompt.iter().all(|status| status.installed);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "initialized": initialized,
                    "engines": statuses,
                    "prompt": prompt,
                }))?
            );
        } else {
            for status in &statuses {
                println!(
                    "{} {:<9} {}",
                    if status.installed { "OK  " } else { "MISS" },
                    status.engine,
                    status.message
                );
            }
            for status in &prompt {
                println!(
                    "{} {:<9} {}",
                    if status.installed { "OK  " } else { "MISS" },
                    format!("prompt/{}", status.shell),
                    status.message
                );
            }
            if initialized {
                println!("hooks initialized for all engines");
            } else {
                println!("hooks missing for some engines; run `a init` to install");
            }
        }
        if !initialized {
            bail!("agent-state hooks are not fully installed");
        }
        return Ok(());
    }

    if args.uninstall {
        let statuses = aplexer::hooks::uninstall(&targets, filter);
        let prompt = aplexer::shell_prompt::uninstall(prompt_ref);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "engines": statuses,
                    "prompt": prompt,
                }))?
            );
        } else {
            for status in &statuses {
                println!("{}: {} — {}", status.engine, status.action, status.message);
            }
            for status in &prompt {
                println!(
                    "prompt/{}: {} — {}",
                    status.shell, status.action, status.message
                );
            }
        }
        return Ok(());
    }

    let statuses = aplexer::hooks::install(&targets, &a_bin, filter);
    let prompt = aplexer::shell_prompt::install(prompt_ref);
    let ok = statuses.iter().all(|status| status.action != "error")
        && prompt.iter().all(|status| status.action != "error");
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": ok,
                "engines": statuses,
                "prompt": prompt,
            }))?
        );
    } else {
        for status in &statuses {
            println!("{}: {} — {}", status.engine, status.action, status.message);
        }
        for status in &prompt {
            println!(
                "prompt/{}: {} — {}",
                status.shell, status.action, status.message
            );
        }
        print_prompt_wiring(&prompt);
    }
    if !ok {
        bail!("agent-state hook installation hit errors");
    }
    Ok(())
}

/// Remind how the managed indicator reaches the visible prompt. Install
/// never rewrites `PS1`/`PROMPT` itself, so without this the block it just
/// wrote sits inert. Printed only when at least one rc file actually holds
/// the block (installed or already present — skipped files need no hint).
fn print_prompt_wiring(prompt: &[aplexer::shell_prompt::PromptStatus]) {
    if !prompt
        .iter()
        .any(|status| status.installed && status.action != "skipped")
    {
        return;
    }
    println!("Add $(__aplexer_indicator) to your prompt to show the session tag, e.g.:");
    println!("  bash: PS1='...$(__aplexer_indicator)...'");
    println!("  zsh:  setopt prompt_subst; PROMPT='...$(__aplexer_indicator)...'");
}
