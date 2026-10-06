//! Git facts about a directory, resolved by invoking `git` with a fixed argv
//! (never a shell, never parsing `.git` files by hand). Both the worktree
//! checkout root and the repository common dir are what "same checkout" vs
//! "related worktree" mean downstream, so both are captured, canonicalized,
//! and stored with declarations.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::canonical_workspace;

/// Where a directory sits inside a git repository. `worktree` is the
/// checkout this directory edits (`git rev-parse --show-toplevel`);
/// `common_dir` is the repository the worktree belongs to
/// (`git rev-parse --git-common-dir`, absolute). Two directories in the same
/// worktree share a checkout; two worktrees of one repository share only the
/// common dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitInfo {
    pub worktree: PathBuf,
    pub common_dir: PathBuf,
}

/// Resolves git facts for `directory`, or `None` when it is not inside a
/// git work tree (including when `git` is missing or too old to answer
/// `--path-format=absolute`). Absence is a fact, not an error: a plain
/// directory workspace has no checkout relations to report.
pub(crate) fn git_info(directory: &Path) -> Option<GitInfo> {
    let worktree = rev_parse(directory, &["rev-parse", "--show-toplevel"])?;
    let common_dir = rev_parse(
        directory,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Some(GitInfo {
        worktree: canonicalize_against(&worktree, directory)?,
        common_dir: canonicalize_against(&common_dir, directory)?,
    })
}

fn rev_parse(directory: &Path, args: &[&str]) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim();
    if line.is_empty() {
        None
    } else {
        Some(PathBuf::from(line))
    }
}

/// Git prints absolute paths here in practice, but an older `git` without
/// `--path-format=absolute` answers `.git`-style relative paths; resolve
/// those against the queried directory. Canonicalizing also aliases through
/// symlinks so two spellings of one repository compare equal.
fn canonicalize_against(path: &Path, base: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        canonical_workspace(path).ok()
    } else {
        canonical_workspace(&base.join(path)).ok()
    }
    .map(strip_verbatim)
}

/// Windows `canonicalize` yields `\\?\C:\x` (or `\\?\UNC\srv\share`); git
/// and users spell these `C:\x`, and stored declarations must compare equal
/// across both spellings. No-op elsewhere.
pub(crate) fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::strip_verbatim;
    use std::path::PathBuf;

    #[test]
    fn strips_verbatim_drive_and_unc_prefixes() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\work\repo")),
            PathBuf::from(r"C:\work\repo")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share\x")),
            PathBuf::from(r"\\srv\share\x")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"C:\plain")),
            PathBuf::from(r"C:\plain")
        );
    }
}
