//! Reading, resolving, and persisting `[shortcuts.<name>]` config entries.

use anyhow::{anyhow, Context, Result};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::ShortcutConfig;
use crate::{atomic_write_bytes, canonical_workspace, ensure_private_dir, Paths};

/// Expand a leading `~` or `~/` against the user's home directory, then
/// canonicalize the workspace path the same way session creation does.
pub fn resolve_shortcut_workspace(path: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(path);
    canonical_workspace(&expanded)
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    if raw == "~" {
        return home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

fn read_config_doc(paths: &Paths) -> Result<(toml_edit::DocumentMut, bool)> {
    let text = match fs::read_to_string(&paths.config_file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((toml_edit::DocumentMut::new(), false));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", paths.config_file.display()));
        }
    };
    let doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {}", paths.config_file.display()))?;
    Ok((doc, true))
}

fn write_config_doc(paths: &Paths, doc: &toml_edit::DocumentMut) -> Result<()> {
    let parent = paths
        .config_file
        .parent()
        .ok_or_else(|| anyhow!("config file has no parent directory"))?;
    ensure_private_dir(parent)?;
    atomic_write_bytes(&paths.config_file, doc.to_string().as_bytes())
}

fn ensure_child_table<'a>(parent: &'a mut toml_edit::Table, key: &str) -> &'a mut toml_edit::Table {
    let needs_table = !matches!(parent.get(key), Some(item) if item.is_table());
    if needs_table {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        parent.insert(key, toml_edit::Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(|item| item.as_table_mut())
        .expect("just inserted table")
}

/// Persist or update `[shortcuts.<name>]` in `config.toml`, preserving
/// existing comments and formatting.
pub fn save_shortcut(paths: &Paths, name: &str, shortcut: &ShortcutConfig) -> Result<()> {
    let (mut doc, _) = read_config_doc(paths)?;
    let shortcuts = ensure_child_table(doc.as_table_mut(), "shortcuts");
    let entry = ensure_child_table(shortcuts, name);
    entry.insert(
        "workspace",
        toml_edit::value(shortcut.workspace.to_string_lossy().into_owned()),
    );
    entry.insert("tag", toml_edit::value(shortcut.tag.as_str()));
    write_config_doc(paths, &doc)
}

/// Remove `[shortcuts.<name>]` from `config.toml`. Returns `true` if the
/// entry existed and was removed.
pub fn remove_shortcut(paths: &Paths, name: &str) -> Result<bool> {
    let (mut doc, existed) = read_config_doc(paths)?;
    if !existed {
        return Ok(false);
    }
    let Some(shortcuts) = doc
        .get_mut("shortcuts")
        .and_then(|item| item.as_table_mut())
    else {
        return Ok(false);
    };
    if shortcuts.remove(name).is_none() {
        return Ok(false);
    }
    if shortcuts.is_empty() {
        doc.remove("shortcuts");
    }
    write_config_doc(paths, &doc)?;
    Ok(true)
}
