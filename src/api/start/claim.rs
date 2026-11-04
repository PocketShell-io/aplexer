//! Claiming a `workspace+tag` for a new session, and resolving the request
//! into the concrete workload the worker will spawn. Both run before
//! anything is written.

use super::*;
use crate::ResolvedLaunch;

/// The `workspace+tag` a start has claimed for its session, decided under
/// the registry lock. `fence` is the pre-PID worker fence over a superseded
/// stub (`_fence`) and must stay alive until `commit_replacement` has archived and
/// deleted that predecessor, so a worker spawned into the stub cannot come
/// up on top of state that is about to be destroyed.
pub(super) struct PairClaim {
    pub(super) tag: String,
    pub(super) superseded: Option<SessionRecord>,
    pub(super) reclaim: Option<ContainmentReap>,
    pub(super) _fence: Option<FileLock>,
}

/// Validate the request and resolve it against the config into the
/// workload the worker will spawn. Nothing here touches the registry.
pub(super) fn resolve_launch(
    paths: &Paths,
    req: &StartRequest,
) -> Result<(PathBuf, ResolvedLaunch)> {
    validate_tag(&req.tag)?;
    if req
        .deadline_ms
        .is_some_and(|deadline| deadline <= crate::now_ms())
    {
        bail!("runtime deadline must be in the future");
    }
    let workspace = canonical_workspace(&req.workspace)?;
    let limits = Limits {
        memory_bytes: req.memory.as_deref().map(parse_byte_size).transpose()?,
        pids: req.pids,
        cpu_quota_us: req.cpu_quota_us,
        cpu_period_us: req.cpu_quota_us.map(|_| req.cpu_period_us),
    };
    let config = Config::load(paths)?;
    let mut launch = config.resolve(
        req.command.clone(),
        req.engine.as_deref(),
        req.profile.as_deref(),
        &workspace,
        req.cwd.as_deref(),
        &req.env,
        &limits,
        req.history_bytes,
    )?;
    if req.command.is_empty() {
        if let Some(model) = req.model.as_deref() {
            apply_model(&mut launch, model)?;
        }
        if !req.no_skip_permissions {
            launch
                .command
                .extend(launch.skip_permissions_argv.iter().cloned());
        }
    } else if req.model.is_some() {
        bail!("--model applies to an engine launch, not a literal command");
    }
    if !command_exists(&launch.command) {
        // Not on PATH: it may be a `~/.bashrc` alias or function, which only
        // an interactive bash can resolve.
        if let Some(wrapped) = bash_alias_argv(&launch.command) {
            launch.command = wrapped;
            return Ok((workspace, launch));
        }
        bail!(
            "command is not executable or was not found in PATH: {}",
            launch
                .command
                .first()
                .map(String::as_str)
                .unwrap_or("<empty>")
        );
    }
    Ok((workspace, launch))
}

/// Select `model` for the resolved engine launch.
///
/// Most engines take `--model <id>`. opencode's top-level TUI command has no
/// `--model` flag (only `opencode run` does), so passing it makes the process
/// print usage and exit 1 at once, and the session vanishes from `a list`.
/// For opencode the model goes through `OPENCODE_CONFIG_CONTENT`, the inline
/// config opencode merges over its files, which every version honours.
pub(crate) fn apply_model(launch: &mut ResolvedLaunch, model: &str) -> Result<()> {
    let is_opencode = launch
        .command
        .first()
        .and_then(|program| Path::new(program).file_stem())
        .is_some_and(|stem| stem == "opencode");
    if !is_opencode {
        return push_model_arg(&mut launch.command, model);
    }
    validate_model_id(model)?;
    let mut config = match launch.env.get(OPENCODE_CONFIG_ENV) {
        Some(existing) if !existing.trim().is_empty() => serde_json::from_str(existing)
            .ok()
            .filter(serde_json::Value::is_object)
            .ok_or_else(|| {
                anyhow!("{OPENCODE_CONFIG_ENV} is not a JSON object; cannot add --model")
            })?,
        _ => serde_json::json!({}),
    };
    config["model"] = serde_json::Value::String(model.to_owned());
    launch
        .env
        .insert(OPENCODE_CONFIG_ENV.to_owned(), config.to_string());
    Ok(())
}

const OPENCODE_CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";

fn validate_model_id(model: &str) -> Result<()> {
    if model.is_empty()
        || model.starts_with('-')
        || model.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        bail!("--model must be one model id, with no spaces or leading dash");
    }
    Ok(())
}

/// Append `--model <model>` as one engine argument.
///
/// The value is a single model id: empty, whitespace, controls, and a
/// leading dash are rejected so the flag cannot become a second option.
pub(crate) fn push_model_arg(command: &mut Vec<String>, model: &str) -> Result<()> {
    validate_model_id(model)?;
    command.push("--model".to_owned());
    command.push(model.to_owned());
    Ok(())
}

/// Rewrite `command` to run through an interactive bash when its first word
/// is a bare name that bash (and so `~/.bashrc`) defines as an alias or
/// function. The name sits at the start of the `-c` text so bash expands the
/// alias at parse time; the remaining arguments pass through as `"$@"`.
fn bash_alias_argv(command: &[String]) -> Option<Vec<String>> {
    bash_alias_argv_with_home(command, None)
}

fn bash_alias_argv_with_home(command: &[String], home: Option<&Path>) -> Option<Vec<String>> {
    let name = command.first()?;
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'));
    if !bare {
        return None;
    }
    // On Windows a bare `bash` on PATH can be the WSL launcher; use the
    // resolved Git-for-Windows bash instead.
    let bash = crate::bash_program()?;
    let mut probe = std::process::Command::new(&bash);
    probe
        .args(["-ic", "type -t -- \"$0\"", name])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(home) = home {
        probe.env("HOME", home);
    }
    let kind = probe.output().ok()?;
    let kind = String::from_utf8_lossy(&kind.stdout);
    // rc files may print banners to stdout before the answer (Ubuntu's
    // /etc/bash.bashrc sudo hint fires whenever HOME lacks its
    // .sudo_as_admin_successful marker, which is always true for the
    // fixture homes here and first-run homes in the wild), and the probe
    // runs after them: the classification is the last non-empty line.
    let kind = kind
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if !matches!(kind, "alias" | "function") {
        return None;
    }
    let mut argv = vec![
        bash,
        "-ic".to_owned(),
        format!("{name} \"$@\""),
        name.clone(),
    ];
    argv.extend(command[1..].iter().cloned());
    Some(argv)
}

/// Decide which pair this start owns and who, if anyone, it supersedes.
/// Must run with the registry lock held: the read here IS the locked read,
/// and the lock stays held through the whole spawn so no other aplexer
/// command can modify the registry until the start returns.
pub(super) fn claim_pair(paths: &Paths, req: &StartRequest, workspace: &Path) -> Result<PairClaim> {
    // Read under the registry lock taken above, and keep holding it through
    // the whole spawn: this read IS the locked read, and no other aplexer
    // command can modify the registry until this call returns.
    let registry = list_records(paths)?;
    // A pair should have exactly one record, but a registry written while
    // the interim #13 fix was live can hold it more than once (rename took
    // the name and left the corpse for `a prune`). Never let "the holder"
    // be whoever `read_dir` lists first: a live holder always wins -- it is
    // who the supersede check below refuses to displace -- and only when
    // every holder is reclaimable does the first dead one become the
    // predecessor this start archives.
    let holder_of = |tag: &str| {
        let mut holders = registry
            .iter()
            .filter(|r| r.workspace == workspace && r.tag == tag);
        holders
            .find(|r| crate::reap_verdict(r).is_none())
            .or_else(|| {
                registry
                    .iter()
                    .find(|r| r.workspace == workspace && r.tag == tag)
            })
    };
    let mut tag = req.tag.clone();
    if req.fresh {
        // `--fresh` promises "always creates": a requested pair held by
        // something live is not an error, it is a reason to move to the next
        // free suffix. A pair that is free, or held only by a record
        // `reap_verdict` would hand over, keeps the exact requested tag --
        // the reclaim path below already owns taking those.
        let Some(chosen) = pick_fresh_tag(&registry, workspace, &req.tag) else {
            bail!(
                "no free tag: every `{0}`, `{0}-2`, `{0}-3`, … candidate in this \
                 workspace is taken or would exceed the tag length limit",
                req.tag
            );
        };
        if chosen != req.tag {
            tag = chosen;
        }
    }
    let superseded = holder_of(&tag).cloned();
    // One shared ownership decision with the worker's `rename` (issue #13):
    // the two commands must not drift apart about when a record has lost
    // its claim -- the disagreement itself was the reported bug.
    let claim = match &superseded {
        Some(existing) => Some(claim_holder_pair(paths, existing)?),
        None => None,
    };
    Ok(PairClaim {
        tag,
        superseded,
        reclaim: claim.as_ref().map(|claim| claim.verdict),
        _fence: claim.and_then(|claim| claim._fence),
    })
}

/// The one `workspace+tag` ownership decision (spec.md 32.1): does the
/// holder of the requested pair still own it? Shared by `claim_pair`'s
/// supersede decision and the worker's `rename` (issue #13) so the two
/// commands cannot drift apart about when a record has lost its claim --
/// the disagreement itself was the reported bug.
///
/// An `Err` names the holder and a way out. An `Ok` means the caller may
/// take the pair and must keep the fence alive across every destruction of
/// the holder's state.
pub(crate) struct HolderClaim {
    /// Whether the holder's worker proved its containment domain empty. A
    /// reclaim without this proof is reported, exactly as `a prune` reports
    /// the same class of removal, because the same manual-investigation
    /// trail goes with it.
    pub(crate) verdict: ContainmentReap,
    /// Held across every destruction so a worker still in the
    /// spawn-to-worker-lock gap cannot come up on top of the state the
    /// caller is about to archive and delete. Never read: the field exists
    /// to be dropped last.
    #[allow(dead_code)]
    pub(crate) _fence: Option<FileLock>,
}

pub(crate) fn claim_holder_pair(paths: &Paths, holder: &SessionRecord) -> Result<HolderClaim> {
    // Taking this pair means archiving and then DELETING the holder's
    // durable state -- the same destruction `a prune` performs -- so it must
    // clear the same bar, `reap_verdict`. `worker_finished()`, the old test,
    // required a terminal phase that a SIGKILLed worker never gets to write,
    // so a zombie (worker dead, `phase` stuck at `running`) held its
    // `workspace+tag` forever.
    let verdict = reap_verdict(holder).ok_or_else(|| {
        anyhow!(
            "workspace+tag already belongs to session {} (state: {}); rename it or choose a different tag",
            holder.id,
            holder.observed_state()
        )
    })?;
    let _fence = fence_or_refuse(paths, holder).with_context(|| {
        format!(
            "workspace+tag already belongs to session {}; rename it or choose a different tag",
            holder.id
        )
    })?;
    Ok(HolderClaim { verdict, _fence })
}

/// Retire a dead holder in one registry-locked step, for a caller that takes
/// the pair without a spawn window in between (the worker's `rename`):
/// archive, delete the archive, drop the runtime directory, and report an
/// unproven reclaim exactly as `a prune` reports the same class of removal.
///
/// `claim_pair` cannot call this in one piece -- its archive must complete
/// before the replacement hand-off and its cleanup only after -- so it
/// drives the same pieces (`archive_reclaimed_predecessor`,
/// `cleanup_superseded_archive`) directly and prints the same warning from
/// the launcher.
pub(crate) fn retire_reclaimed_holder(
    paths: &Paths,
    holder: &SessionRecord,
    verdict: ContainmentReap,
) -> Result<()> {
    // Preserve the crash evidence before the archive is deleted (intake
    // 5246e591): the warning while the record still exists, the tombstone
    // after the archive is gone -- same as `commit_replacement`.
    let diagnostic_error = preserve_reclaimed_evidence(paths, holder);
    let archived = archive_reclaimed_predecessor(paths, holder)?;
    if let Err(error) = cleanup_superseded_archive(&archived) {
        bail!(
            "superseded session {} cleanup failed; its remaining evidence is retained at {}: {error:#}",
            holder.id,
            archived.display()
        );
    }
    let _ = fs::remove_dir_all(paths.runtime_session(holder.id));
    write_reclaimed_tombstone(paths, holder, diagnostic_error.as_deref());
    if verdict == ContainmentReap::NoRemainingHandle {
        eprintln!(
            "a: reclaimed workspace+tag from broken session {} without a containment proof; its worker died without recording one and nothing addressable remained",
            holder.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod model_arg_tests {
    use super::{apply_model, push_model_arg, OPENCODE_CONFIG_ENV};
    use crate::ResolvedLaunch;

    fn launch(command: &[&str]) -> ResolvedLaunch {
        ResolvedLaunch {
            engine: "x".into(),
            profile: None,
            command: command.iter().map(|s| s.to_string()).collect(),
            cwd: ".".into(),
            env: Default::default(),
            env_unset: vec![],
            skip_permissions_argv: vec![],
            limits: Default::default(),
            history_bytes: 0,
        }
    }

    #[test]
    fn opencode_model_goes_through_inline_config_not_a_flag() {
        let mut l = launch(&["/usr/bin/opencode"]);
        apply_model(&mut l, "opencode-go/muse-spark-1.3-contributor").unwrap();
        assert_eq!(l.command, vec!["/usr/bin/opencode".to_string()]);
        let cfg: serde_json::Value = serde_json::from_str(&l.env[OPENCODE_CONFIG_ENV]).unwrap();
        assert_eq!(cfg["model"], "opencode-go/muse-spark-1.3-contributor");
    }

    #[test]
    fn opencode_model_merges_into_existing_inline_config() {
        let mut l = launch(&["opencode"]);
        l.env.insert(
            OPENCODE_CONFIG_ENV.into(),
            r#"{"theme":"x","model":"old"}"#.into(),
        );
        apply_model(&mut l, "p/m").unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&l.env[OPENCODE_CONFIG_ENV]).unwrap();
        assert_eq!(
            (cfg["theme"].as_str(), cfg["model"].as_str()),
            (Some("x"), Some("p/m"))
        );
        l.env.insert(OPENCODE_CONFIG_ENV.into(), "not json".into());
        assert!(apply_model(&mut l, "p/m").is_err());
        assert!(apply_model(&mut launch(&["opencode"]), "--bad").is_err());
    }

    #[test]
    fn other_engines_still_get_the_model_flag() {
        let mut l = launch(&["codex"]);
        apply_model(&mut l, "gpt-5.4").unwrap();
        assert_eq!(
            l.command,
            vec!["codex".to_string(), "--model".into(), "gpt-5.4".into()]
        );
        assert!(l.env.is_empty());
    }

    #[test]
    fn model_is_one_argument_before_later_flags() {
        let mut command = vec![
            "codex".into(),
            "-c".into(),
            "check_for_update_on_startup=false".into(),
        ];
        push_model_arg(&mut command, "gpt-5.4").unwrap();
        command.push("--dangerously-bypass-approvals-and-sandbox".into());
        assert_eq!(
            command,
            vec![
                "codex".to_string(),
                "-c".into(),
                "check_for_update_on_startup=false".into(),
                "--model".into(),
                "gpt-5.4".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
            ]
        );
    }

    #[test]
    fn model_rejects_empty_space_and_a_leading_dash() {
        let mut command = vec!["claude".into()];
        for bad in ["", "gpt 5", "gpt\n5", "--fast"] {
            assert!(push_model_arg(&mut command, bad).is_err(), "{bad}");
        }
        assert_eq!(command, vec!["claude".to_string()]);
    }
}

#[cfg(test)]
mod bash_alias_tests {
    use super::*;

    #[test]
    fn bashrc_alias_is_wrapped_and_unknown_names_are_not() {
        let Some(bash) = crate::bash_program() else {
            eprintln!("no bash available; skipping");
            return;
        };
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".bashrc"),
            "alias zzaliasprobe='echo hi'\n",
        )
        .unwrap();
        let cmd = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let wrapped =
            bash_alias_argv_with_home(&cmd(&["zzaliasprobe", "a b"]), Some(home.path())).unwrap();
        assert_eq!(
            wrapped,
            cmd(&[&bash, "-ic", "zzaliasprobe \"$@\"", "zzaliasprobe", "a b"])
        );
        assert!(bash_alias_argv_with_home(&cmd(&["zznosuchcmd"]), Some(home.path())).is_none());
        assert!(bash_alias_argv_with_home(&cmd(&["./zzaliasprobe"]), Some(home.path())).is_none());
    }
}
