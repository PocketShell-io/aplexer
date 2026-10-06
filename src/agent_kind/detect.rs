//! The `/proc` descendant walk that turns comm/cmdline evidence into a
//! detected agent, plus the profile-resolution read off the process's
//! environment.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

use super::rules::classify_token_detailed;
use super::{AgentKind, DetectedAgent, ProfileVariants};

/// Safety bound on a single detection walk. A session's workload subtree is
/// a handful of processes; this only stops a pathological (or hostile) tree
/// from turning a `a list --json` into an unbounded `/proc` scan.
const MAX_SCANNED_PIDS: usize = 4096;

/// `/proc/<pid>/comm`, trimmed. `None` when the pid is gone or unreadable.
#[cfg(not(windows))]
fn read_comm(proc_root: &Path, pid: u32) -> Option<String> {
    let text = fs::read_to_string(proc_root.join(pid.to_string()).join("comm")).ok()?;
    Some(text.trim().to_owned())
}

/// `/proc/<pid>/cmdline` (NUL-delimited) joined with spaces. `None` when the
/// pid is gone or unreadable; an empty string for a kernel thread.
#[cfg(not(windows))]
fn read_cmdline(proc_root: &Path, pid: u32) -> Option<String> {
    let raw = fs::read(proc_root.join(pid.to_string()).join("cmdline")).ok()?;
    let decoded = String::from_utf8_lossy(&raw);
    Some(
        decoded
            .split('\0')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_owned(),
    )
}

/// The value of `var` in `/proc/<pid>/environ` (NUL-delimited `KEY=value`
/// words). `None` when the pid is gone, unreadable, or does not carry the
/// variable -- the same defensive read every other /proc probe here does.
#[cfg(not(windows))]
fn read_environ_var(proc_root: &Path, pid: u32, var: &str) -> Option<String> {
    let raw = fs::read(proc_root.join(pid.to_string()).join("environ")).ok()?;
    let prefix = format!("{var}=");
    String::from_utf8_lossy(&raw)
        .split('\0')
        .find(|word| word.starts_with(&prefix))
        .map(|word| word[prefix.len()..].to_owned())
}

/// Windows backends: the process image name (`.exe` stripped), command line
/// and environment come from `sys::windows::procinfo`; `proc_root` is unused.
#[cfg(windows)]
fn read_comm(_proc_root: &Path, pid: u32) -> Option<String> {
    crate::sys::windows::procinfo::image_name(pid)
}

/// Command line normalised so the whole-word rules apply: backslashes become
/// `/` and `.exe` is dropped (`C:\x\claude.exe` -> `C:/x/claude`).
#[cfg(windows)]
fn read_cmdline(_proc_root: &Path, pid: u32) -> Option<String> {
    let raw = crate::sys::windows::procinfo::cmdline(pid)?;
    Some(normalize_windows_cmdline(&raw))
}

#[cfg(windows)]
fn read_environ_var(_proc_root: &Path, pid: u32, var: &str) -> Option<String> {
    crate::sys::windows::procinfo::environ_var(pid, var)
}

#[cfg(windows)]
pub(super) fn normalize_windows_cmdline(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.replace('\\', "/");
    while let Some(i) = rest.to_ascii_lowercase().find(".exe") {
        let after = rest[i + 4..].chars().next();
        if after.is_some_and(|c| c.is_alphanumeric()) {
            out.push_str(&rest[..i + 4]);
        } else {
            out.push_str(&rest[..i]);
        }
        rest = rest[i + 4..].to_owned();
    }
    out.push_str(&rest);
    out.trim().to_owned()
}

#[cfg(not(windows))]
fn children_of(proc_root: &Path, pid: u32) -> Vec<u32> {
    crate::worker::direct_child_pids_in(proc_root, pid).unwrap_or_default()
}

#[cfg(windows)]
fn children_of(_proc_root: &Path, pid: u32) -> Vec<u32> {
    crate::sys::windows::procinfo::children(pid)
}

/// The profile id of the variation `kind` is running as on `pid`, from the
/// same evidence `config::discovery` registers profiles from:
///
/// * A non-default profile env (`CODEX_HOME`/`CLAUDE_CONFIG_DIR`, per
///   `config::discovery`'s rule table) wins: its dir's stem minus the
///   leading dot is the profile id, matching discovery's keying exactly. An
///   env value pointing at the *default* dir names no variation, so the
///   token rule still gets its say.
/// * Otherwise the variation token the process was classified by
///   (`profile_variants`): a session running a configured variation's
///   binary is that profile even with no env override -- the usual
///   hand-launched shape.
/// * Anything else is the engine's own default config: `None`.
///
/// Agents without a profile env rule (opencode, grok) can only ever be the
/// default profile or a configured variation token, so their env arm never
/// fires.
fn resolve_profile(
    proc_root: &Path,
    pid: u32,
    kind: AgentKind,
    variant: Option<&str>,
) -> Option<String> {
    if let Some((env_var, default_dirname)) = kind.profile_env() {
        if let Some(dir) = read_environ_var(proc_root, pid, env_var) {
            let stem = Path::new(&dir)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.trim_start_matches('.').to_owned())
                .filter(|stem| !stem.is_empty());
            if stem.as_deref() != Some(default_dirname.trim_start_matches('.')) {
                return stem;
            }
        }
    }
    variant.map(str::to_owned)
}

/// Classify one pid by its `comm` first (cheap and definitive for an
/// unwrapped CLI) and then its `cmdline` (which catches the node-wrapped
/// form whose comm is just `node`).
fn classify_pid(
    proc_root: &Path,
    pid: u32,
    variants: &ProfileVariants,
) -> Option<(AgentKind, Option<String>)> {
    if let Some(comm) = read_comm(proc_root, pid) {
        if let Some(named) = classify_token_detailed(&comm, variants) {
            return Some(named);
        }
    }
    let cmdline = read_cmdline(proc_root, pid)?;
    if cmdline.is_empty() {
        return None;
    }
    classify_token_detailed(&cmdline, variants)
}

/// The agent running in `workload_pid`'s process tree, or `None`.
///
/// The walk is breadth-first from the workload leader itself (a session
/// started directly as `a start -- claude` has the agent AS its workload,
/// while a shell session has it one or more levels below), visiting each
/// level's children in ascending pid order so the answer is deterministic
/// rather than dependent on directory-read order. The first pid whose
/// comm/cmdline names an agent wins.
///
/// `variants` is [`super::profile_variants`] over the caller's loaded
/// config -- the variation tokens (`zcodex`, a user profile's executable,
/// ...) whose binary names classify as that variation. Canonical agent
/// commands need no entry and empty variants simply detect canonical agents
/// only.
///
/// Never fails: an unreadable pid, a `children` file that vanished
/// mid-walk, or a pid that exited between enumeration and classification is
/// skipped. `None` means "no agent found", which is also the honest answer
/// for a workload that is no longer alive.
pub fn detect_agent(
    proc_root: &Path,
    workload_pid: u32,
    variants: &ProfileVariants,
) -> Option<AgentKind> {
    detect_agent_detailed(proc_root, workload_pid, variants).map(|detected| detected.kind)
}

/// `detect_agent` plus the variation the agent runs as (`DetectedAgent`).
pub fn detect_agent_detailed(
    proc_root: &Path,
    workload_pid: u32,
    variants: &ProfileVariants,
) -> Option<DetectedAgent> {
    detect_agent_process(proc_root, workload_pid, variants).map(|agent| agent.detected)
}

/// The process an agent was detected in, not just what it is: the pid is the
/// identity-backed anchor for reading the agent's own open native-log file
/// descriptors (issue #20's exact automatic binding) -- the top-level
/// agent's pid, deliberately, so a nested agent's rollout can never be
/// mistaken for the session's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcess {
    pub pid: u32,
    pub detected: DetectedAgent,
}

/// `detect_agent_detailed` plus the pid the answer was classified from.
pub fn detect_agent_process(
    proc_root: &Path,
    workload_pid: u32,
    variants: &ProfileVariants,
) -> Option<AgentProcess> {
    let mut pending = VecDeque::from([workload_pid]);
    let mut seen = HashSet::from([workload_pid]);
    let mut scanned = 0usize;
    while let Some(pid) = pending.pop_front() {
        scanned += 1;
        if scanned > MAX_SCANNED_PIDS {
            return None;
        }
        if let Some((kind, variant)) = classify_pid(proc_root, pid, variants) {
            return Some(AgentProcess {
                pid,
                detected: DetectedAgent {
                    kind,
                    profile: resolve_profile(proc_root, pid, kind, variant.as_deref()),
                },
            });
        }
        // A read error here is "this pid told us nothing", never a failure:
        // the strict, error-propagating variant is the containment walker's
        // contract (a truncated kill list is a bug), not detection's.
        let mut children = children_of(proc_root, pid);
        children.sort_unstable();
        for child in children {
            if seen.insert(child) {
                pending.push_back(child);
            }
        }
    }
    None
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn cmdline_is_normalised_for_the_token_rules() {
        assert_eq!(
            normalize_windows_cmdline(r#""C:\Tools\Claude.EXE" --resume"#),
            r#""C:/Tools/Claude" --resume"#
        );
        assert_eq!(normalize_windows_cmdline(r"C:\x\a.exec"), "C:/x/a.exec");
        let variants = ProfileVariants::default();
        let text = normalize_windows_cmdline(
            r"node.exe C:\npm\node_modules\@anthropic-ai\claude-code\cli.js",
        );
        assert!(classify_token_detailed(&text, &variants).is_some());
    }

    #[test]
    fn image_name_of_a_live_child_classifies() {
        // `cmd` is not an agent, but the walk must read it without panicking.
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 3 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let found = detect_agent(
            Path::new("/proc"),
            std::process::id(),
            &ProfileVariants::default(),
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(found.is_none());
    }
}
