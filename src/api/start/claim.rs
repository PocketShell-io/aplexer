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
    if req.command.is_empty() && !req.no_skip_permissions {
        launch
            .command
            .extend(launch.skip_permissions_argv.iter().cloned());
    }
    if !command_exists(&launch.command) {
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
    let archived = archive_reclaimed_predecessor(paths, holder)?;
    if let Err(error) = cleanup_superseded_archive(&archived) {
        bail!(
            "superseded session {} cleanup failed; its remaining evidence is retained at {}: {error:#}",
            holder.id,
            archived.display()
        );
    }
    let _ = fs::remove_dir_all(paths.runtime_session(holder.id));
    if verdict == ContainmentReap::NoRemainingHandle {
        eprintln!(
            "a: reclaimed workspace+tag from broken session {} without a containment proof; its worker died without recording one and nothing addressable remained",
            holder.id
        );
    }
    Ok(())
}
