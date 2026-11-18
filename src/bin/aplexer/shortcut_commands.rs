use super::*;

pub(crate) fn validate_shortcut_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("shortcut name must contain 1..64 bytes");
    }
    if name.starts_with('-') {
        bail!("shortcut name must not start with '-'");
    }
    if name.bytes().all(|b| b.is_ascii_digit()) {
        bail!("shortcut name must not be all digits (reserved for `a <N>` quick-attach)");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("shortcut name may contain only ASCII letters, digits, '.', '_' and '-'");
    }
    if is_reserved_cli_word(name) {
        bail!("shortcut name {name:?} is reserved by an existing subcommand or alias");
    }
    Ok(())
}

pub(crate) fn cmd_shortcut(paths: &Paths, args: ShortcutArgs, json_output: bool) -> Result<()> {
    match args.command.unwrap_or(ShortcutCommand::List) {
        ShortcutCommand::List => cmd_shortcut_list(paths, json_output),
        ShortcutCommand::Add(add) => cmd_shortcut_add(paths, add, json_output),
        ShortcutCommand::Remove(rm) => cmd_shortcut_remove(paths, rm, json_output),
    }
}

fn cmd_shortcut_list(paths: &Paths, json_output: bool) -> Result<()> {
    let config = Config::load(paths)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&config.shortcuts)?);
        return Ok(());
    }
    if config.shortcuts.is_empty() {
        println!("no configured shortcuts");
        return Ok(());
    }
    let width = config
        .shortcuts
        .keys()
        .map(|k| k.len())
        .max()
        .unwrap_or(0)
        .max(12);
    for (name, shortcut) in &config.shortcuts {
        println!(
            "{:<width$}  {}:{}",
            name,
            shortcut.workspace.display(),
            shortcut.tag,
            width = width
        );
    }
    Ok(())
}

fn cmd_shortcut_add(paths: &Paths, args: ShortcutAddArgs, json_output: bool) -> Result<()> {
    validate_shortcut_name(&args.name)?;
    validate_tag(&args.tag)?;
    let raw_workspace = args.workspace.unwrap_or_else(|| PathBuf::from("."));
    let workspace = resolve_shortcut_workspace(&raw_workspace)?;
    let shortcut = ShortcutConfig {
        workspace,
        tag: args.tag,
    };
    save_shortcut(paths, &args.name, &shortcut)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "name": args.name,
                "workspace": shortcut.workspace,
                "tag": shortcut.tag,
            }))?
        );
    } else {
        println!(
            "shortcut {} -> {}:{}",
            args.name,
            shortcut.workspace.display(),
            shortcut.tag
        );
    }
    Ok(())
}

fn cmd_shortcut_remove(paths: &Paths, args: ShortcutRemoveArgs, json_output: bool) -> Result<()> {
    if !remove_shortcut(paths, &args.name)? {
        bail!("no shortcut named {:?}", args.name);
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "removed": args.name,
            }))?
        );
    } else {
        println!("removed shortcut {}", args.name);
    }
    Ok(())
}

pub(crate) fn cmd_quick_shortcut(paths: &Paths, args: QuickShortcutArgs) -> Result<()> {
    let config = Config::load(paths)?;
    let shortcut = config
        .shortcuts
        .get(&args.name)
        .ok_or_else(|| anyhow!("unknown shortcut {:?}", args.name))?;
    let workspace = resolve_shortcut_workspace(&shortcut.workspace)?;
    if let Some(existing) = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == shortcut.tag)
    {
        if existing.worker_phase_active() && existing.worker_alive() {
            return attach(paths, &existing, None, false, false);
        }
    }
    cmd_start(
        paths,
        StartArgs {
            workspace,
            tag: shortcut.tag.clone(),
            engine: Some("shell".to_string()),
            model: None,
            profile: None,
            cwd: None,
            env: vec![],
            memory: None,
            pids: None,
            cpu_quota_us: None,
            cpu_period_us: 100_000,
            history_bytes: None,
            attach: true,
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            deadline_ms: None,
            no_skip_permissions: false,
            fresh: false,
            label: vec![],
            command: vec![],
        },
        false,
    )
}
