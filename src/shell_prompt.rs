//! `a init`'s shell-prompt half: a `[tag]` indicator for `PS1`/`PROMPT`.
//!
//! A session's tag is stamped into `APLEXER_TAG` once, at spawn
//! (`worker::spawn_workload`), and never updated afterwards — so a prompt
//! that prints `$APLEXER_TAG` keeps showing the pre-`a rename` tag forever.
//! The worker *does* rewrite `session.json` on every rename, so the fix is
//! to resolve the tag live (via `a whoami --json`, which reads that record)
//! and fall back to the env var only when the lookup fails. The format is
//! `[tag]`, without the old `a:` prefix.
//!
//! This module owns the snippet text and the marker-block management `a
//! init` applies to the user's rc files (`~/.bashrc`, `~/.zshrc`):
//!
//! - Install is additive and idempotent. Only files that already exist are
//!   touched (no rc file is ever invented for a shell the user may not
//!   run); the block is appended once and rewritten only when its content
//!   differs (no mtime churn). Anything outside the markers — including a
//!   hand-written `__aplexer_indicator` predating this — is left alone; the
//!   managed block is appended last, so it wins duplicate definitions.
//! - `a init` deliberately does *not* rewire `PS1`/`PROMPT` itself: the
//!   prompt string is the user's, and rewriting it automatically is exactly
//!   the clobber this tooling avoids everywhere else. Install prints the
//!   one-line wiring instead.
//! - The snippet is POSIX-`sh` compatible (`local` aside, which both bash
//!   and zsh accept), so one text serves both shells. Fish is out of scope
//!   for now: its prompt protocol differs entirely.
//!
//! Block grammar: everything from the `BEGIN` marker line through the `END`
//! marker line, inclusive, is ours. A `BEGIN` without an `END` is treated
//! as stale (the block is replaced from `BEGIN` to end-of-file).

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Marker lines bracketing our rc-file block. Grep-able and distinct from
/// anything a user would write by hand.
pub const PROMPT_BEGIN: &str = "# >>> aplexer prompt >>>";
pub const PROMPT_END: &str = "# <<< aplexer prompt <<<";

/// The indicator function both shells source. Resolves the tag live so `a
/// rename` reflects on the next prompt; `$APLEXER_TAG` is only the fallback
/// (worker spawn stamps it, rename never updates it).
pub const PROMPT_SNIPPET: &str = r##"__aplexer_indicator() {
  # Live lookup so `a rename` reflects immediately: APLEXER_TAG is stamped
  # at spawn and never updated, while the worker rewrites session.json on
  # every rename. Falls back to the env var when the lookup fails.
  local tag=""
  if [ -n "$APLEXER_SESSION_ID" ] && command -v a >/dev/null 2>&1; then
    tag=$(a whoami --json 2>/dev/null | sed -n 's/.*"tag"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  fi
  [ -z "$tag" ] && tag="$APLEXER_TAG"
  [ -n "$tag" ] && printf ' \033[38;2;198;120;221m[%s]\033[00m' "$tag"
  return 0
}"##;

/// The exact block `a init` manages, markers included. The comment carries
/// the PS1/PROMPT wiring because install never rewrites the prompt string
/// itself — the user adds `$(__aplexer_indicator)` once, by hand.
pub fn managed_block() -> String {
    format!(
        "{PROMPT_BEGIN}\n\
         # Managed by `a init` (re-run to refresh; `a init --uninstall` removes).\n\
         # Shows the current session's tag as `[tag]` (live: `a rename`\n\
         # reflects on the next prompt). Wire it into your prompt, e.g.:\n\
         #   bash: PS1='...$(__aplexer_indicator)...'\n\
         #   zsh:  setopt prompt_subst; PROMPT='...$(__aplexer_indicator)...'\n\
         {PROMPT_SNIPPET}\n\
         {PROMPT_END}"
    )
}

/// Whether an rc file's block is current, stale, or absent. A `BEGIN`
/// without an `END` counts as stale (replaced from `BEGIN` to EOF).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Current,
    Stale,
    Absent,
}

/// Locate our block as line indexes `(begin, end)` inclusive. `end` is
/// `None` when `BEGIN` has no closing `END`.
fn find_block_lines(lines: &[&str]) -> Option<(usize, Option<usize>)> {
    let begin = lines.iter().position(|line| line.trim() == PROMPT_BEGIN)?;
    let end = lines[begin..]
        .iter()
        .position(|line| line.trim() == PROMPT_END)
        .map(|offset| begin + offset);
    Some((begin, end))
}

pub fn block_state(existing: &str) -> BlockState {
    let lines: Vec<&str> = existing.lines().collect();
    let Some((begin, end)) = find_block_lines(&lines) else {
        return BlockState::Absent;
    };
    let Some(end) = end else {
        return BlockState::Stale;
    };
    let current_block = managed_block();
    let current: Vec<&str> = current_block.lines().collect();
    if lines[begin..=end] == current[..] {
        BlockState::Current
    } else {
        BlockState::Stale
    }
}

/// Join lines with a single trailing newline (empty stays empty), so every
/// write is deterministic and `write_if_changed`-style comparisons hold.
fn join_lines(lines: &[&str]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Install (or refresh) the block. Returns the new content and whether it
/// changed. Content outside the markers is preserved byte-for-byte apart
/// from trailing-newline normalisation.
pub fn install_block(existing: &str) -> (String, bool) {
    if block_state(existing) == BlockState::Current {
        return (existing.to_string(), false);
    }
    let block = managed_block();
    let block_lines: Vec<&str> = block.lines().collect();
    let lines: Vec<&str> = existing.lines().collect();
    let next = match find_block_lines(&lines) {
        Some((begin, Some(end))) => {
            let mut next = lines[..begin].to_vec();
            next.extend(block_lines);
            next.extend_from_slice(&lines[end + 1..]);
            next
        }
        // Orphaned BEGIN: replace it and everything after it.
        Some((begin, None)) => {
            let mut next = lines[..begin].to_vec();
            next.extend(block_lines);
            next
        }
        None => {
            let mut next = lines;
            if !next.is_empty() {
                next.push("");
            }
            next.extend(block_lines);
            next
        }
    };
    (join_lines(&next), true)
}

/// Remove the block. Returns the new content and whether it changed.
/// Everything outside the markers is preserved.
pub fn remove_block(existing: &str) -> (String, bool) {
    let lines: Vec<&str> = existing.lines().collect();
    let Some((begin, end)) = find_block_lines(&lines) else {
        return (existing.to_string(), false);
    };
    let end = end.unwrap_or(lines.len().saturating_sub(1).max(begin));
    let mut next = lines[..begin].to_vec();
    next.extend_from_slice(&lines[end + 1..]);
    // Collapse blank lines left at the joint, then normalise the tail.
    let mut tidy: Vec<&str> = Vec::with_capacity(next.len());
    for line in next {
        if line.trim().is_empty() && tidy.last().is_none_or(|last: &&str| last.trim().is_empty()) {
            continue;
        }
        tidy.push(line);
    }
    (
        join_lines(&tidy).trim_end().to_string() + if tidy.is_empty() { "" } else { "\n" },
        true,
    )
}

// ---------------------------------------------------------------------------
// rc-file targets + install/check/uninstall drivers (mirrors hooks::drivers)
// ---------------------------------------------------------------------------

/// One shell rc file under management.
#[derive(Debug, Clone)]
pub struct PromptRc {
    /// `bash` or `zsh` (selects nothing today — one snippet serves both —
    /// but keeps statuses and messages precise).
    pub shell: &'static str,
    pub path: PathBuf,
}

/// Every rc file `a init` manages, resolved from `$HOME`. `home` is a
/// parameter rather than read here so tests can point at a throwaway dir
/// (same split as `hooks::resolve_targets`).
pub fn prompt_targets(home: &Path) -> Vec<PromptRc> {
    vec![
        PromptRc {
            shell: "bash",
            path: home.join(".bashrc"),
        },
        PromptRc {
            shell: "zsh",
            path: home.join(".zshrc"),
        },
    ]
}

/// Production resolution: `$HOME` from the environment.
pub fn prompt_targets_from_env() -> Result<Vec<PromptRc>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(prompt_targets(&home))
}

/// One rc file's install/check/uninstall outcome. Serializable so `a init
/// --json` / `a init --check --json` can carry it additively next to the
/// per-engine statuses.
#[derive(Debug, Clone, Serialize)]
pub struct PromptStatus {
    pub shell: String,
    pub installed: bool,
    pub action: String,
    pub message: String,
    pub paths: Vec<String>,
}

impl PromptStatus {
    fn new(shell: &str, installed: bool, action: &str, message: String, path: &Path) -> Self {
        Self {
            shell: shell.to_string(),
            installed,
            action: action.to_string(),
            message,
            paths: vec![path.display().to_string()],
        }
    }
}

/// Whether the file defines the indicator outside our markers (a
/// hand-written predecessor). Install leaves it alone but says so: the
/// managed block is appended last and therefore wins, yet the user should
/// know to delete the stale copy. Matches definitions only
/// (`name()` with any leading `function` keyword and spacing), never mere
/// uses like `$(__aplexer_indicator)` in `PS1`.
fn has_unmanaged_indicator(text: &str) -> bool {
    fn is_definition(line: &str) -> bool {
        let line = line.trim().trim_start_matches("function").trim_start();
        line.starts_with("__aplexer_indicator()")
    }
    let lines: Vec<&str> = text.lines().collect();
    match find_block_lines(&lines) {
        Some((begin, Some(end))) => lines[..begin]
            .iter()
            .chain(lines[end + 1..].iter())
            .any(|line| is_definition(line)),
        // Orphaned BEGIN: everything from it to EOF is replaced on
        // install, so only the head can hold an unmanaged copy.
        Some((begin, None)) => lines[..begin].iter().any(|line| is_definition(line)),
        None => text.lines().any(is_definition),
    }
}

/// Install the block into every existing rc file. Files that do not exist
/// are skipped, never created: there is no rc to manage for a shell the
/// user does not run. Writes go through the hooks file layer (symlink-aware
/// for dotfiles-managed rcs, mode-preserving, no-op when unchanged).
pub fn install(targets: &[PromptRc]) -> Vec<PromptStatus> {
    targets
        .iter()
        .map(|rc| {
            if !rc.path.exists() {
                return PromptStatus::new(
                    rc.shell,
                    true,
                    "skipped",
                    format!("{} not present; nothing to manage", rc.path.display()),
                    &rc.path,
                );
            }
            match fs::read_to_string(&rc.path)
                .with_context(|| format!("read {}", rc.path.display()))
                .map(|text| {
                    let unmanaged = has_unmanaged_indicator(&text);
                    let (next, changed) = install_block(&text);
                    (next, changed, unmanaged)
                })
                .and_then(|(next, changed, unmanaged)| {
                    crate::hooks::write_if_changed(&rc.path, &next).map(|_| (changed, unmanaged))
                }) {
                Ok((false, _)) => PromptStatus::new(
                    rc.shell,
                    true,
                    "present",
                    format!("prompt block already installed in {}", rc.path.display()),
                    &rc.path,
                ),
                Ok((true, unmanaged)) => {
                    let mut message =
                        format!("installed prompt block in {}", rc.path.display());
                    if unmanaged {
                        message += " (a hand-written __aplexer_indicator elsewhere in the file was left alone; the managed block is last so it wins — delete the old copy)";
                    }
                    PromptStatus::new(rc.shell, true, "installed", message, &rc.path)
                }
                Err(e) => PromptStatus::new(rc.shell, false, "error", format!("{e:#}"), &rc.path),
            }
        })
        .collect()
}

/// Check block presence. No files are touched; missing rc files count as
/// satisfied (nothing to manage), so headless hosts without rc files still
/// report initialised.
pub fn check(targets: &[PromptRc]) -> Vec<PromptStatus> {
    targets
        .iter()
        .map(|rc| {
            if !rc.path.exists() {
                return PromptStatus::new(
                    rc.shell,
                    true,
                    "skipped",
                    format!("{} not present; nothing to manage", rc.path.display()),
                    &rc.path,
                );
            }
            match fs::read_to_string(&rc.path) {
                Err(e) => PromptStatus::new(
                    rc.shell,
                    false,
                    "error",
                    format!("read {}: {e:#}", rc.path.display()),
                    &rc.path,
                ),
                Ok(text) => match block_state(&text) {
                    BlockState::Current => PromptStatus::new(
                        rc.shell,
                        true,
                        "present",
                        format!("prompt block installed in {}", rc.path.display()),
                        &rc.path,
                    ),
                    BlockState::Stale => PromptStatus::new(
                        rc.shell,
                        false,
                        "absent",
                        format!(
                            "{} holds an outdated prompt block; re-run `a init`",
                            rc.path.display()
                        ),
                        &rc.path,
                    ),
                    BlockState::Absent => PromptStatus::new(
                        rc.shell,
                        false,
                        "absent",
                        format!(
                            "{} has no prompt block; run `a init` then add $(__aplexer_indicator) to your prompt",
                            rc.path.display()
                        ),
                        &rc.path,
                    ),
                },
            }
        })
        .collect()
}

/// Remove our blocks. Only marker-bracketed lines go; everything else —
/// including hand-written predecessors — stays.
pub fn uninstall(targets: &[PromptRc]) -> Vec<PromptStatus> {
    targets
        .iter()
        .map(|rc| {
            if !rc.path.exists() {
                return PromptStatus::new(
                    rc.shell,
                    false,
                    "absent",
                    format!("{} not present", rc.path.display()),
                    &rc.path,
                );
            }
            match fs::read_to_string(&rc.path)
                .with_context(|| format!("read {}", rc.path.display()))
                .map(|text| remove_block(&text))
                .and_then(|(next, changed)| {
                    crate::hooks::write_if_changed(&rc.path, &next).map(|_| changed)
                }) {
                Ok(true) => PromptStatus::new(
                    rc.shell,
                    false,
                    "removed",
                    format!("removed prompt block from {}", rc.path.display()),
                    &rc.path,
                ),
                Ok(false) => PromptStatus::new(
                    rc.shell,
                    false,
                    "absent",
                    format!("no prompt block in {}", rc.path.display()),
                    &rc.path,
                ),
                Err(e) => PromptStatus::new(rc.shell, false, "error", format!("{e:#}"), &rc.path),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD_BLOCK: &str = "# >>> aplexer prompt >>>\n__aplexer_indicator() {\n  [ -n \"$APLEXER_TAG\" ] && printf ' [a:%s]' \"$APLEXER_TAG\"\n}\n# <<< aplexer prompt <<<";

    #[test]
    fn snippet_shows_tag_without_prefix_and_looks_up_live() {
        assert!(
            PROMPT_SNIPPET.contains("a whoami --json"),
            "must resolve live so renames reflect"
        );
        assert!(PROMPT_SNIPPET.contains("[%s]"), "must print [tag]");
        assert!(
            !PROMPT_SNIPPET.contains("[a:%s]"),
            "must not print the old a: prefix"
        );
    }

    #[test]
    fn install_appends_block_once_and_is_idempotent() {
        let existing = "export PATH=\"$HOME/bin:$PATH\"\n";
        let (once, changed) = install_block(existing);
        assert!(changed);
        assert!(once.starts_with(existing));
        assert!(once.contains(PROMPT_BEGIN));
        assert!(once.ends_with(&format!("{PROMPT_END}\n")));
        let (twice, changed) = install_block(&once);
        assert!(!changed);
        assert_eq!(once, twice);
    }

    #[test]
    fn install_into_empty_file_writes_just_the_block() {
        let (next, changed) = install_block("");
        assert!(changed);
        assert_eq!(next, managed_block() + "\n");
        assert_eq!(block_state(&next), BlockState::Current);
    }

    #[test]
    fn install_replaces_a_stale_block_in_place() {
        let existing = format!("first=1\n{OLD_BLOCK}\nlast=2\n");
        assert_eq!(block_state(&existing), BlockState::Stale);
        let (next, changed) = install_block(&existing);
        assert!(changed);
        assert!(next.starts_with("first=1\n"));
        assert!(next.ends_with("last=2\n"));
        assert!(!next.contains("[a:%s]"));
        assert_eq!(block_state(&next), BlockState::Current);
    }

    #[test]
    fn install_leaves_unmanaged_content_alone() {
        let hand = "__aplexer_indicator() {\n  echo hand-written\n}\n";
        let (next, changed) = install_block(hand);
        assert!(changed);
        // Both survive; the managed block sorts last so it wins.
        assert!(next.contains("echo hand-written"));
        assert!(next.contains(PROMPT_BEGIN));
        assert!(next.rfind(PROMPT_BEGIN) > next.rfind("echo hand-written"));
    }

    #[test]
    fn uninstall_removes_only_the_block() {
        let existing = format!("first=1\n{OLD_BLOCK}\nlast=2\n");
        let (next, changed) = remove_block(&existing);
        assert!(changed);
        assert_eq!(next, "first=1\nlast=2\n");
        let (again, changed) = remove_block(&next);
        assert!(!changed);
        assert_eq!(again, next);
    }

    #[test]
    fn uninstall_without_block_changes_nothing() {
        let existing = "export PATH=\"$HOME/bin:$PATH\"\n";
        let (next, changed) = remove_block(existing);
        assert!(!changed);
        assert_eq!(next, existing);
    }

    #[test]
    fn orphaned_begin_counts_as_stale_and_is_replaced() {
        let existing = "keep=1\n# >>> aplexer prompt >>>\n__aplexer_indicator() {\n";
        assert_eq!(block_state(existing), BlockState::Stale);
        let (next, changed) = install_block(existing);
        assert!(changed);
        assert!(next.starts_with("keep=1\n"));
        assert_eq!(block_state(&next), BlockState::Current);
    }

    #[test]
    fn prompt_use_in_ps1_is_not_an_unmanaged_definition() {
        let rc = "PS1='...$(__aplexer_indicator)...'\n";
        assert!(!has_unmanaged_indicator(rc));
        let with_block = install_block(rc).0;
        // Install stays silent about predecessors when there are none.
        assert!(!has_unmanaged_indicator(&with_block));
        let hand = "__aplexer_indicator() {\n  echo old\n}\n";
        assert!(has_unmanaged_indicator(hand));
        assert!(has_unmanaged_indicator(&format!(
            "{hand}PS1='$(__aplexer_indicator)'\n{}",
            managed_block()
        )));
    }

    #[test]
    fn prompt_targets_cover_bash_and_zsh() {
        let targets = prompt_targets(Path::new("/home/u"));
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].shell, "bash");
        assert_eq!(targets[0].path, PathBuf::from("/home/u/.bashrc"));
        assert_eq!(targets[1].shell, "zsh");
        assert_eq!(targets[1].path, PathBuf::from("/home/u/.zshrc"));
    }

    #[test]
    fn missing_rc_files_are_skipped_never_created() {
        let dir = tempfile::tempdir().unwrap();
        let targets = prompt_targets(dir.path());
        for status in install(&targets) {
            assert_eq!(status.action, "skipped");
            assert!(status.installed);
        }
        assert!(!targets[0].path.exists());
        assert!(!targets[1].path.exists());
        // ...and a skip still counts as satisfied for --check.
        assert!(check(&targets).iter().all(|s| s.installed));
    }

    #[test]
    fn install_check_uninstall_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        std::fs::write(&rc, "export FOO=1\n").unwrap();
        let targets = vec![PromptRc {
            shell: "bash",
            path: rc.clone(),
        }];
        let installed = install(&targets);
        assert_eq!(installed[0].action, "installed");
        assert!(installed[0].installed);
        assert!(check(&targets).iter().all(|s| s.installed));

        // Second install is a no-op (no mtime churn).
        let before = std::fs::metadata(&rc).unwrap().modified().unwrap();
        let again = install(&targets);
        assert_eq!(again[0].action, "present");
        assert_eq!(std::fs::metadata(&rc).unwrap().modified().unwrap(), before);

        let removed = uninstall(&targets);
        assert_eq!(removed[0].action, "removed");
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "export FOO=1\n");
        assert!(!check(&targets).iter().all(|s| s.installed));
    }
}
