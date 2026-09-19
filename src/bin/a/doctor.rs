use super::*;

#[derive(Debug)]
pub(crate) struct CgroupLimitProbe {
    pub(crate) cgroup_v2: bool,
    pub(crate) controllers: Vec<String>,
    pub(crate) delegated_scope: bool,
    pub(crate) detail: String,
}

/// The read-only half of the probe: is cgroup v2 mounted, and does it
/// expose every controller a limited session needs? `Err` carries the
/// finished probe verdict for whichever gap it found, so the caller can
/// report it without touching systemd.
fn controller_gaps() -> Result<Vec<String>, CgroupLimitProbe> {
    if let Err(error) = current_cgroup_identity() {
        return Err(CgroupLimitProbe {
            cgroup_v2: false,
            controllers: Vec::new(),
            delegated_scope: false,
            detail: format!("cgroup v2 unavailable: {error:#}"),
        });
    }

    let controllers_path = Path::new("/sys/fs/cgroup/cgroup.controllers");
    let controllers: Vec<String> = match fs::read_to_string(controllers_path) {
        Ok(value) => value.split_whitespace().map(str::to_string).collect(),
        Err(error) => {
            return Err(CgroupLimitProbe {
                cgroup_v2: true,
                controllers: Vec::new(),
                delegated_scope: false,
                detail: format!("cannot read {}: {error}", controllers_path.display()),
            });
        }
    };
    let required_controllers = ["cpu", "memory", "pids"];
    let missing: Vec<&str> = required_controllers
        .into_iter()
        .filter(|required| !controllers.iter().any(|found| found == required))
        .collect();
    if !missing.is_empty() {
        return Err(CgroupLimitProbe {
            cgroup_v2: true,
            controllers,
            delegated_scope: false,
            detail: format!(
                "cgroup v2 is mounted but required controller(s) are absent: {}",
                missing.join(", ")
            ),
        });
    }
    Ok(controllers)
}

/// Exercise the exact launch implementation with a short-lived placeholder
/// workload: trusted systemd-run/systemctl/sleep discovery, the systemd
/// --user manager, Delegate=yes, member placement by the manager, and all
/// three supported controllers. The scope contains only the probe's
/// `sleep` process and is cleaned immediately; no existing cgroup or
/// workload is modified.
fn probe_delegated_scope(controllers: Vec<String>) -> CgroupLimitProbe {
    let verdict = |delegated_scope: bool, detail: String| CgroupLimitProbe {
        cgroup_v2: true,
        controllers: controllers.clone(),
        delegated_scope,
        detail,
    };
    let probe_limits = Limits {
        memory_bytes: Some(64 * 1024 * 1024),
        pids: Some(16),
        cpu_quota_us: Some(10_000),
        cpu_period_us: Some(100_000),
    };
    match probe_placeholder_scope(&probe_limits) {
        Ok(()) => verdict(
            true,
            "verified a temporary delegated systemd --user scope with memory, pids, and cpu controls".into(),
        ),
        Err(error) => verdict(
            false,
            format!("delegated systemd --user scope probe failed: {error:#}"),
        ),
    }
}

pub(crate) fn probe_cgroup_limits() -> CgroupLimitProbe {
    match controller_gaps() {
        Err(probe) => probe,
        Ok(controllers) => probe_delegated_scope(controllers),
    }
}

pub(crate) fn cgroup_limits_check(probe: CgroupLimitProbe) -> Value {
    let required_controllers = ["cpu", "memory", "pids"];
    let controllers_ok = required_controllers
        .iter()
        .all(|required| probe.controllers.iter().any(|found| found == required));
    let available = probe.cgroup_v2 && controllers_ok && probe.delegated_scope;
    let detail = if available {
        probe.detail.clone()
    } else {
        format!(
            "{}; resource limits unavailable, but unlimited sessions still work",
            probe.detail
        )
    };
    json!({
        "name": "cgroup_limits",
        "ok": available,
        "severity": if available { "ok" } else { "warning" },
        "required": false,
        "available": available,
        "detail": detail,
        "prerequisites": {
            "cgroup_v2": probe.cgroup_v2,
            "controllers": {
                "ok": controllers_ok,
                "required": required_controllers,
                "available": probe.controllers,
            },
            "delegated_systemd_user_scope": {
                "ok": probe.delegated_scope,
                "detail": probe.detail,
                "method": "temporary_scope_via_launch_path",
                "verifies": [
                    "trusted_systemd_run_systemctl_sleep",
                    "systemd_user_manager",
                    "delegate_yes",
                    "writable_cgroup_procs",
                ],
            },
        },
    })
}

pub(crate) fn doctor_checks_ok(checks: &[Value]) -> bool {
    checks
        .iter()
        .all(|check| check["ok"].as_bool().unwrap_or(false) || check["severity"] == "warning")
}

/// The active recorded sessions sitting in the per-user manager's exit
/// failure domain, from the `worker_cgroup` evidence each worker records
/// at launch.
fn vulnerable_session_entries(paths: &Paths) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::new();
    if let Ok(records) = list_records(paths) {
        for record in records {
            if !record.worker_phase_active() {
                continue;
            }
            let vulnerable = record
                .worker_cgroup
                .as_deref()
                .map(|cgroup| {
                    aplexer::placement::classify_cgroup_path(cgroup)
                        .vulnerable_to_user_manager_exit()
                })
                .unwrap_or(false);
            if vulnerable {
                entries.push(json!({
                    "id": record.id.to_string(),
                    "selector": record.selector(),
                    "worker_cgroup": record.worker_cgroup,
                }));
            }
        }
    }
    entries
}

/// The `launch_placement` doctor check (issue #1). Two questions in one:
/// (1) which service manager owns the cgroup this process is running in --
/// the placement every `a start` launched from this context hands its
/// worker, since setsid() changes session, not cgroup -- and (2) how many
/// active recorded sessions sit in the per-user manager's exit.target
/// failure domain, from the `worker_cgroup` evidence the worker now records
/// at launch. Warning-severity by design: the issue asks aplexer to warn
/// clearly, and a vulnerable placement has actionable workarounds (launch
/// context, or the opt-in system scope), so it must not fail the host.
pub(crate) fn launch_placement_check(paths: &Paths) -> Value {
    let own_cgroup = aplexer::placement::read_process_cgroup(std::process::id());
    let own_placement = own_cgroup
        .as_deref()
        .map(aplexer::placement::classify_cgroup_path);
    let vulnerable = own_placement
        .map(|placement| placement.vulnerable_to_user_manager_exit())
        .unwrap_or(false);
    let vulnerable_sessions = vulnerable_session_entries(paths);
    let placement_name = own_placement.map(|placement| placement.name());
    let advice = own_placement.and_then(|placement| placement.advice());
    let mut detail = format!(
        "aplexer commands launched here run in cgroup {} ({})",
        own_cgroup.as_deref().unwrap_or("<unknown>"),
        placement_name.unwrap_or("unknown"),
    );
    if vulnerable {
        if let Some(advice) = advice {
            detail.push_str(&format!(
                "; sessions started here will die at `systemctl --user exit`; {advice}"
            ));
        }
    } else if let Some(advice) = advice {
        detail.push_str(&format!("; note: {advice}"));
    }
    if !vulnerable_sessions.is_empty() {
        detail.push_str(&format!(
            "; {} active session(s) recorded inside the per-user manager failure domain",
            vulnerable_sessions.len()
        ));
    }
    json!({
        "name": "launch_placement",
        "ok": !vulnerable,
        "severity": if vulnerable { "warning" } else { "ok" },
        "required": false,
        "detail": detail,
        "own_cgroup": own_cgroup,
        "own_placement": placement_name,
        "vulnerable_to_user_manager_exit": vulnerable,
        "vulnerable_sessions": vulnerable_sessions,
        "advice": advice,
        // Doctor only reads /proc and session records; it never probes the
        // system-scope backend (that would create a transient scope just by
        // asking for a checkup). The escape is documented here, and its
        // availability is proven at the opted-in `a start` that uses it.
        "escape": {
            "env": aplexer::placement::LAUNCH_SYSTEM_SCOPE_ENV,
            "value": aplexer::placement::LAUNCH_SYSTEM_SCOPE_VALUE,
            "requested": aplexer::placement::system_scope_requested(),
        },
    })
}

fn unix_socket_path_check(paths: &Paths) -> Value {
    let sample = paths.socket(Uuid::nil());
    let sample_fits = sample.as_os_str().len() < 108;
    json!({
        "name": "unix_socket_path",
        "ok": sample_fits,
        "detail": sample.display().to_string(),
    })
}

fn config_check(paths: &Paths) -> Value {
    match Config::load(paths) {
        Ok(config) => json!({
            "name": "config",
            "ok": true,
            "detail": format!(
                "{} engines, {} profiles",
                config.engines.len(),
                config.profiles.len()
            ),
        }),
        Err(e) => json!({"name": "config", "ok": false, "detail": format!("{e:#}")}),
    }
}

/// One row of the `engine_resolution` check's `executables` array.
fn engine_resolution_row_json(row: &ResolutionRow) -> Value {
    json!({
        "kind": row.kind,
        "name": row.name,
        "executable": row.executable,
        "verdict": row.verdict.as_str(),
        "current": row.current.as_ref().map(|path| path.display().to_string()),
        "minimal": row.minimal.as_ref().map(|path| path.display().to_string()),
        "fixable": row.wants_fix(),
        "detail": row.detail,
    })
}

/// The `engine_resolution` doctor check (issue #19). Every configured
/// engine `command[0]` and profile `executable`/`command[0]` is probed
/// under (a) the invoking shell's PATH and (b) a minimal non-interactive
/// session PATH (system dirs + `~/.local/bin`). A bare name only (a) can
/// resolve is exactly what the app's non-interactive SSH hits as "not
/// found in PATH" — `~/.bashrc` never sources nvm there — and the durable
/// fix is `a doctor --fix` pinning the resolved absolute path into the
/// config file. A pinned absolute path whose file has since vanished is
/// flagged as stale (version-manager drift). Warning-severity by design:
/// an engine that is merely not installed is a legitimate state, and a
/// flagged one still launches fine from the shell it resolves in.
fn engine_resolution_check(paths: &Paths) -> Value {
    let minimal = minimal_path();
    let config = match Config::load(paths) {
        Ok(config) => config,
        Err(error) => {
            return json!({
                "name": "engine_resolution",
                "ok": false,
                "severity": "warning",
                "required": false,
                "detail": format!(
                    "config did not load ({error:#}); engine PATH resolution not probed"
                ),
                "minimal_path": minimal,
                "executables": [],
            });
        }
    };
    let current_path = env::var("PATH").unwrap_or_default();
    let rows = resolution_rows(&config, &current_path, &minimal);
    let flagged: Vec<String> = rows
        .iter()
        .filter(|row| row.wants_fix())
        .map(|row| format!("{} {}", row.kind, row.name))
        .collect();
    let not_installed: Vec<String> = rows
        .iter()
        .filter(|row| row.verdict == ResolutionVerdict::NotInstalled)
        .map(|row| format!("{} {}", row.kind, row.name))
        .collect();
    let mut detail = if flagged.is_empty() {
        format!(
            "{} engine(s), {} profile(s): every command resolves under a non-interactive session's PATH (or is pinned to an existing path)",
            config.engines.len(),
            config.profiles.len()
        )
    } else {
        format!(
            "a non-interactive session (the app's SSH) cannot resolve {} — run `a doctor --fix` to pin absolute paths into the config file",
            flagged.join(", ")
        )
    };
    if !not_installed.is_empty() {
        detail.push_str(&format!("; not installed: {}", not_installed.join(", ")));
    }
    json!({
        "name": "engine_resolution",
        "ok": flagged.is_empty(),
        "severity": if flagged.is_empty() { "ok" } else { "warning" },
        "required": false,
        "detail": detail,
        "minimal_path": minimal,
        "executables": rows.iter().map(engine_resolution_row_json).collect::<Vec<_>>(),
    })
}

/// The host-level checks that do not touch session records: platform,
/// durable roots, socket path length, cgroup capability, launch placement,
/// config load, engine PATH resolution.
fn environment_checks(paths: &Paths) -> Vec<Value> {
    vec![
        json!({"name":"linux","ok":true,"detail":std::env::consts::OS}),
        path_check("runtime_root", &paths.runtime_root),
        path_check("state_root", &paths.state_root),
        unix_socket_path_check(paths),
        cgroup_limits_check(probe_cgroup_limits()),
        launch_placement_check(paths),
        config_check(paths),
        engine_resolution_check(paths),
    ]
}

/// One active record that is neither alive-and-reachable nor a startup
/// still in flight: the broken/stale evidence the sessions check reports,
/// plus whether `a prune` can reap it. Recovery advice has to follow the
/// same predicate prune actually uses, or doctor sends the user at a
/// command that hard-fails. `a kill` on a broken unlimited record
/// exits 1 with "no authoritative containment locator",
/// and `a forget --force`'s "workload processes may
/// survive" warning is not what this needs -- for a
/// record prune can reap, `a prune` is the whole answer.
fn broken_session_entry(record: SessionRecord) -> Option<(Value, bool)> {
    if !record.worker_phase_active() {
        return None;
    }
    let worker_alive = record.worker_alive();
    let rpc_error = rpc_simple(&record, Operation::Status, None)
        .err()
        .map(|error| format!("{error:#}"));
    let worker_reachable = rpc_error.is_none();
    if worker_alive && worker_reachable {
        return None;
    }
    let state = derived_liveness(&record.phase, worker_alive, record.created_at_ms);
    // A `Starting` record inside the startup window has no
    // worker pid yet and no socket to answer an RPC: that is
    // `a start` in flight, not wreckage. Reporting it here
    // sent the user at `a prune` / `a kill` for a session
    // that was about to come up on its own (issue #9).
    if state == "starting" {
        return None;
    }
    let reapable = reap_verdict(&record).is_some();
    let recovery = if reapable {
        json!({ "prune": "a prune" })
    } else {
        json!({
            "kill": format!("a kill {}", record.id),
            "forget": format!("a forget {} --force", record.id),
        })
    };
    Some((
        json!({
            "id": record.id,
            "selector": record.selector(),
            "phase": record.phase.name(),
            "state": state,
            "worker_alive": worker_alive,
            "worker_reachable": worker_reachable,
            "rpc_error": rpc_error,
            "recovery": recovery,
        }),
        reapable,
    ))
}

/// The one-line sessions summary, split out for the four advisory paths a
/// registry can take: clean, all reapable, none reapable, mixed.
fn sessions_detail(record_count: usize, broken_count: usize, reapable_count: usize) -> String {
    if broken_count == 0 {
        format!("{record_count} session record(s), none broken")
    } else if reapable_count == broken_count {
        format!("{broken_count} broken/stale session(s), all reapable; run `a prune`")
    } else if reapable_count == 0 {
        format!(
            "{} broken/stale session(s); run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`",
            broken_count
        )
    } else {
        format!(
            "{} broken/stale session(s); `a prune` removes {reapable_count} of them, for the rest run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`",
            broken_count
        )
    }
}

fn sessions_health_check(paths: &Paths) -> Value {
    match list_records(paths) {
        Ok(records) => {
            let record_count = records.len();
            let mut reapable_count = 0usize;
            let mut broken = Vec::new();
            for (entry, reapable) in records.into_iter().filter_map(broken_session_entry) {
                if reapable {
                    reapable_count += 1;
                }
                broken.push(entry);
            }
            json!({
                "name": "sessions",
                "ok": broken.is_empty(),
                "detail": sessions_detail(record_count, broken.len(), reapable_count),
                "broken_sessions": broken,
            })
        }
        Err(error) => json!({
            "name": "sessions",
            "ok": false,
            "detail": format!("cannot inspect session records: {error:#}"),
            "broken_sessions": [],
        }),
    }
}

fn print_doctor_checks(checks: &[Value]) -> Result<()> {
    for check in checks {
        let label = if check["severity"] == "warning" {
            "WARN"
        } else if check["ok"].as_bool().unwrap_or(false) {
            "OK"
        } else {
            "FAIL"
        };
        let name = check["name"]
            .as_str()
            .ok_or_else(|| anyhow!("doctor check without a name: {check}"))?;
        println!(
            "{:<5} {:<20} {}",
            label,
            name,
            check["detail"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// Load the merged config, probe both PATHs, and persist a fix candidate
/// for every pin-worthy row (`aplexer::config::apply_pins`).
fn apply_engine_pins(paths: &Paths) -> Result<(Vec<AppliedPin>, Vec<String>)> {
    let config = Config::load(paths)?;
    let current_path = env::var("PATH").unwrap_or_default();
    let rows = resolution_rows(&config, &current_path, &minimal_path());
    apply_pins(paths, &config, &rows)
}

pub(crate) fn cmd_doctor(paths: &Paths, fix: bool, json_output: bool) -> Result<()> {
    let mut checks = environment_checks(paths);
    checks.push(sessions_health_check(paths));
    let mut fix_report = json!(null);
    if fix {
        let (applied, failures) = apply_engine_pins(paths)?;
        // The engine_resolution check above probed the pre-fix state; make
        // this run's output reflect what `--fix` actually left behind.
        if let Some(slot) = checks
            .iter_mut()
            .find(|check| check["name"] == "engine_resolution")
        {
            *slot = engine_resolution_check(paths);
        }
        fix_report = json!({
            "applied": applied
                .iter()
                .map(|pin| json!({
                    "target": pin.target,
                    "from": pin.from,
                    "to": pin.to.display().to_string(),
                }))
                .collect::<Vec<_>>(),
            "failed": failures,
        });
        if !json_output {
            for pin in &applied {
                println!("pinned {}: {} → {}", pin.target, pin.from, pin.to.display());
            }
            for failure in &failures {
                println!("{failure}");
            }
            if applied.is_empty() && failures.is_empty() {
                println!("nothing to fix");
            }
        }
    }
    let warnings = checks
        .iter()
        .filter(|check| check["severity"] == "warning")
        .count();
    let ok = doctor_checks_ok(&checks);
    if json_output {
        let mut report = json!({"ok":ok,"warnings":warnings,"checks":checks});
        if !fix_report.is_null() {
            report["fix"] = fix_report;
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_doctor_checks(&checks)?;
    }
    if !ok {
        bail!("one or more doctor checks failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_detail_reports_a_clean_registry() {
        assert_eq!(sessions_detail(3, 0, 0), "3 session record(s), none broken");
    }

    #[test]
    fn sessions_detail_sends_all_reapable_records_to_prune() {
        assert_eq!(
            sessions_detail(3, 2, 2),
            "2 broken/stale session(s), all reapable; run `a prune`"
        );
    }

    #[test]
    fn sessions_detail_names_kill_and_forget_when_prune_cannot_reap() {
        assert_eq!(
            sessions_detail(3, 1, 0),
            "1 broken/stale session(s); run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`"
        );
    }

    #[test]
    fn sessions_detail_splits_the_mixed_registry_between_prune_and_kill() {
        assert_eq!(
            sessions_detail(3, 2, 1),
            "2 broken/stale session(s); `a prune` removes 1 of them, for the rest run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`"
        );
    }
}
