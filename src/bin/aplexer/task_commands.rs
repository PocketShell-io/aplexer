use super::*;

pub(crate) fn cmd_task(paths: &Paths, args: TaskRunArgs, json_output: bool) -> Result<()> {
    let started_ms = now_ms();
    let task_id = Uuid::now_v7();

    // The real parent: the session this process runs inside, discovered from
    // the ambient APLEXER_SESSION_ID (walking ancestor environments when a
    // tool subprocess cleared it) and confirmed against a live record. There
    // is no flag for this on purpose -- a task's parent can only be a real
    // session it actually belongs to.
    let parent = current_task_parent(paths)?;

    let cwd = canonical_workspace(args.cwd.as_deref().unwrap_or(Path::new(".")))?;
    let config = Config::load(paths)?;
    let env_overrides = parse_env(&args.env)?;
    // Resolution is exactly `a start`'s: engine command, profile
    // executable/account, env, provider-key strip, skip-permissions argv.
    // The task never names a shell alias or interpolates a command line.
    // Resolve once to learn the engine this launch would use (explicit
    // --engine, else the profile's engine, else the configured default),
    // apply cutoff routing to that, and re-resolve if routing changed it.
    let resolve_launch = |engine: Option<&str>| {
        config.resolve(
            Vec::new(),
            engine,
            args.profile.as_deref(),
            &cwd,
            args.cwd.as_deref(),
            &env_overrides,
            &Limits::default(),
            None,
        )
    };
    let default_launch = resolve_launch(args.engine.as_deref())?;
    let engine = aplexer::task::route_engine(
        &default_launch.engine,
        args.cutoff.as_deref(),
        args.cutoff_engine.as_deref(),
        started_ms,
    )?;
    let launch = if engine == default_launch.engine {
        default_launch
    } else {
        resolve_launch(Some(&engine))?
    };

    // The noninteractive mode argv: engine config wins, built-in family
    // table otherwise, and a refusal when neither knows the engine's
    // noninteractive flags -- guessing would be the shell-glue mistake again.
    let task_argv = config
        .engines
        .get(&engine)
        .map(|engine_config| {
            aplexer::task::noninteractive_argv(
                &engine,
                if engine_config.task_argv.is_empty() {
                    None
                } else {
                    Some(&engine_config.task_argv)
                },
            )
        })
        .unwrap_or_else(|| aplexer::task::noninteractive_argv(&engine, None))
        .ok_or_else(|| {
            anyhow!(
                "engine {engine:?} declares no noninteractive task argv; set \
                 [engines.{engine}] task_argv (e.g. task_argv = [\"-p\"]) in the aplexer config"
            )
        })?;

    let prompt_bytes_raw = fs::read(&args.prompt_file)
        .with_context(|| format!("read prompt file {}", args.prompt_file.display()))?;
    let prompt = String::from_utf8(prompt_bytes_raw.clone()).with_context(|| {
        format!(
            "prompt file {} must be valid UTF-8",
            args.prompt_file.display()
        )
    })?;
    let digest = prompt_digest(&prompt_bytes_raw);
    let prompt_bytes = prompt_bytes_raw.len() as u64;

    // Child argv: engine command (+ profile), the noninteractive mode argv,
    // skip-permissions argv unless opted out, and the caller's engine args.
    // The prompt text is appended verbatim as the final element only on the
    // child's copy -- the START/RESULT records carry the prompt-free argv
    // plus its size/digest fingerprint.
    let mut argv = launch.command.clone();
    argv.extend(task_argv);
    if !args.no_skip_permissions {
        argv.extend(launch.skip_permissions_argv.clone());
    }
    argv.extend(args.engine_args.iter().cloned());
    let mut child_argv = argv.clone();
    child_argv.push(prompt);
    let record_argv = || aplexer::task::record_argv(argv.clone(), prompt_bytes, &digest);

    let output_dir = match &args.output_dir {
        Some(dir) => dir.clone(),
        None => aplexer::task::default_output_dir(&launch.cwd, &engine, started_ms, task_id),
    };
    if output_dir.exists() && !args.overwrite && aplexer::task::result_path(&output_dir).exists() {
        bail!(
            "output directory {} already holds a RESULT.json; pass --overwrite to run again",
            output_dir.display()
        );
    }
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("create task output directory {}", output_dir.display()))?;
    let stdout_log = output_dir.join("stdout.log");
    let stderr_log = output_dir.join("stderr.log");

    let started_at = aplexer::task::rfc3339_utc(started_ms);
    let start_record = aplexer::task::TaskStartRecord {
        schema_version: aplexer::task::TASK_RECORD_SCHEMA_VERSION,
        task_id,
        engine: engine.clone(),
        profile: launch.profile.clone(),
        argv: record_argv(),
        prompt_path: args.prompt_file.clone(),
        prompt_bytes,
        prompt_sha256: digest.clone(),
        cwd: launch.cwd.clone(),
        output_dir: output_dir.clone(),
        parent_session: parent.clone(),
        started_at: started_at.clone(),
        started_ms,
    };
    aplexer::task::write_task_record(
        &aplexer::task::start_record_path(&output_dir),
        &start_record,
    )?;

    let timeout = args
        .timeout_secs
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs);
    let outcome = match aplexer::task::run_task_child(
        &child_argv,
        &launch.cwd,
        &launch.env,
        &launch.env_unset,
        &stdout_log,
        &stderr_log,
        timeout,
    ) {
        Ok(outcome) => outcome,
        // Could not even start (engine binary missing, output files
        // uncreatable): keep the evidence honest in RESULT.json and exit
        // 127, the conventional "command could not be launched".
        Err(error) => {
            let ended_ms = now_ms();
            let result = aplexer::task::TaskResultRecord {
                schema_version: aplexer::task::TASK_RECORD_SCHEMA_VERSION,
                task_id,
                engine: engine.clone(),
                profile: launch.profile.clone(),
                argv: record_argv(),
                cwd: launch.cwd.clone(),
                output_dir: output_dir.clone(),
                stdout_log,
                stderr_log,
                prompt_path: args.prompt_file.clone(),
                prompt_bytes,
                prompt_sha256: digest,
                parent_session: parent.clone(),
                started_at,
                started_ms,
                ended_at: aplexer::task::rfc3339_utc(ended_ms),
                ended_ms,
                exit_code: 127,
                exit_signal: None,
                timed_out: false,
                error: Some(format!("{error:#}")),
                notice: aplexer::task::NoticeRecord::skipped(
                    "failed",
                    format!("task child never started: {error:#}"),
                ),
            };
            aplexer::task::write_task_record(&aplexer::task::result_path(&output_dir), &result)?;
            print_result(&result, json_output);
            // The child never ran, so 127 is this command's status too: a
            // failed launch must not read as success to the hosting script.
            aplexer::task::flush_stdout();
            std::process::exit(result.exit_code);
        }
    };

    let ended_ms = now_ms();
    let mut result = aplexer::task::TaskResultRecord {
        schema_version: aplexer::task::TASK_RECORD_SCHEMA_VERSION,
        task_id,
        engine: engine.clone(),
        profile: launch.profile.clone(),
        argv: record_argv(),
        cwd: launch.cwd.clone(),
        output_dir: output_dir.clone(),
        stdout_log,
        stderr_log,
        prompt_path: args.prompt_file.clone(),
        prompt_bytes,
        prompt_sha256: digest,
        parent_session: parent,
        started_at,
        started_ms,
        ended_at: aplexer::task::rfc3339_utc(ended_ms),
        ended_ms,
        exit_code: outcome.exit_code,
        exit_signal: outcome.exit_signal,
        timed_out: outcome.timed_out,
        error: None,
        // Filled in below; replaced before the record is ever written.
        notice: aplexer::task::NoticeRecord::skipped("pending", String::new()),
    };

    if !args.no_notify {
        result.notice = send_task_notice(paths, &args, &result);
    } else {
        result.notice = aplexer::task::NoticeRecord::skipped("disabled", "--no-notify".into());
    }

    aplexer::task::write_task_record(&aplexer::task::result_path(&output_dir), &result)?;
    print_result(&result, json_output);

    // The task's real status is this command's status: a failed task must
    // not look like a successful run to whatever script hosted it.
    aplexer::task::flush_stdout();
    std::process::exit(result.exit_code);
}

fn print_result(result: &aplexer::task::TaskResultRecord, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    let status = if result.timed_out {
        "timed out".to_string()
    } else {
        format!("exit {}", result.exit_code)
    };
    println!("task {}: {status}", result.task_id);
    println!("engine: {}", result.engine);
    println!("output: {}", result.output_dir.display());
    match result.notice.status.as_str() {
        "sent" => println!("notice: {}", result.notice.message_id.expect("sent has id")),
        other => match &result.notice.detail {
            Some(detail) => println!("notice: {other} ({detail})"),
            None => println!("notice: {other}"),
        },
    }
}

/// The calling session as `ParentSession`, or `None` when this process does
/// not run inside a live aplexer session (a bare terminal run): the task
/// still works, it just records and reports no parent.
fn current_task_parent(paths: &Paths) -> Result<Option<aplexer::task::ParentSession>> {
    let Some(id) = discover_session_id() else {
        return Ok(None);
    };
    Ok(read_record(&paths.record(id))
        .ok()
        .map(|record| aplexer::task::ParentSession {
            id: record.id,
            tag: record.tag,
            workspace: record.workspace,
        }))
}

/// The durable completion notice, sent through the ordinary mailbox with the
/// calling session's own identity -- the same rules as
/// `a message send --workspace`: a real session record is required, `--from`
/// is never faked, and an unknown target tag is refused. A notice problem
/// never discards the task result; it is recorded on the result instead.
fn send_task_notice(
    paths: &Paths,
    args: &TaskRunArgs,
    result: &aplexer::task::TaskResultRecord,
) -> aplexer::task::NoticeRecord {
    let send = || -> Result<Uuid> {
        let records = list_records(paths)?;
        // Identity first, classified explicitly: no ambient session id at
        // all is "no-session-identity"; an id without a record here (a stale
        // or foreign stamp) is "identity-unresolved". Both are honest skips
        // -- the runner glue's identity-mismatch case -- never a faked
        // sender.
        let Some(id) = discover_session_id() else {
            return Err(NoticeSkip::NoSessionIdentity.into());
        };
        let Some(record) = records.iter().find(|record| record.id == id) else {
            return Err(NoticeSkip::IdentityUnresolved(
                format!("calling session {id} has no record in this state directory").into(),
            )
            .into());
        };
        let from = MessageFrom {
            session_id: Some(record.id),
            workspace: Some(record.workspace.clone()),
            tag: Some(record.tag.clone()),
            engine: Some(record.engine.clone()),
            profile: record.profile.clone(),
            external: false,
        };
        let workspace = match &args.notify_workspace {
            Some(workspace) => canonical_workspace(workspace)?,
            // Default: the calling session's own workspace (its record, not
            // a stale env var).
            None => record.workspace.clone(),
        };
        let to = build_recipient(
            &records,
            &workspace,
            Some(args.notify_to.as_deref().unwrap_or("main")),
            false,
            None,
            false,
        )?;
        let envelope = MessageEnvelope {
            schema_version: MESSAGE_SCHEMA_VERSION,
            id: Uuid::now_v7(),
            workspace: workspace.clone(),
            created_at: now_secs(),
            from,
            to,
            kind: "task-result".to_string(),
            reply_to: None,
            body: aplexer::task::notice_body(&result.engine, result),
            data: Some(aplexer::task::notice_data(result)),
            delivery: Delivery::Inbox,
        };
        check_body_size(&envelope.body)?;
        let mp = ensure_workspace(paths, &workspace)?;
        let sent = finish_send(
            &mp,
            &records,
            &workspace,
            envelope,
            &PaneDeliveryArgs {
                pane: false,
                or_inbox: false,
                raw: false,
                no_enter: false,
            },
        )?;
        Ok(sent.id)
    };
    match send() {
        Ok(id) => aplexer::task::NoticeRecord {
            status: "sent".to_string(),
            message_id: Some(id),
            detail: None,
        },
        Err(error) => {
            let (status, detail) = match error.downcast::<NoticeSkip>() {
                Ok(NoticeSkip::NoSessionIdentity) => (
                    "no-session-identity".to_string(),
                    "no APLEXER_SESSION_ID in this environment or its ancestors".to_string(),
                ),
                Ok(NoticeSkip::IdentityUnresolved(detail)) => {
                    ("identity-unresolved".to_string(), detail.to_string())
                }
                Err(error) => ("failed".to_string(), format!("{error:#}")),
            };
            aplexer::task::NoticeRecord {
                status,
                message_id: None,
                detail: Some(detail),
            }
        }
    }
}

/// Why a completion notice was skipped rather than sent. These are the two
/// expected no-real-identity shapes; anything else is a `failed` notice and
/// keeps the full error chain.
#[derive(Debug)]
enum NoticeSkip {
    NoSessionIdentity,
    IdentityUnresolved(Box<str>),
}

impl std::fmt::Display for NoticeSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoticeSkip::NoSessionIdentity => write!(f, "no session identity"),
            NoticeSkip::IdentityUnresolved(detail) => write!(f, "identity unresolved: {detail}"),
        }
    }
}

impl std::error::Error for NoticeSkip {}

/// SHA-256 of the prompt bytes, for the record fingerprint.
fn prompt_digest(prompt: &[u8]) -> String {
    aplexer::task::sha256_hex(prompt)
}
