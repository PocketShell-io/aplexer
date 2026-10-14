//! Workspace-trust preparation: ensure newly created worktrees and workspaces
//! have their directory trust pre-accepted for Claude Code and Codex / zcodex
//! before the workload spawns, so startup trust prompts never swallow input
//! or block unattended agent sessions (spec.md §8.2, §26).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use toml_edit::{DocumentMut, Item, Table};

use crate::canonical_workspace;
use crate::config::engine_family;

fn ensure_child_table<'a>(table: &'a mut Table, key: &str) -> &'a mut Table {
    if !matches!(table.get(key), Some(item) if item.is_table()) {
        table.insert(key, Item::Table(Table::new()));
    }
    table
        .get_mut(key)
        .unwrap()
        .as_table_mut()
        .expect("just ensured a table")
}

pub fn update_claude_trust(
    claude_json_path: &Path,
    dirs: &[PathBuf],
    create_if_missing: bool,
) -> Result<bool> {
    if !claude_json_path.exists() && !create_if_missing {
        return Ok(false);
    }
    let mut root: serde_json::Value = if claude_json_path.exists() {
        let content = fs::read_to_string(claude_json_path)
            .with_context(|| format!("read {}", claude_json_path.display()))?;
        if content.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&content)
                .with_context(|| format!("parse JSON from {}", claude_json_path.display()))?
        }
    } else {
        serde_json::json!({})
    };

    let obj = root.as_object_mut().ok_or_else(|| {
        anyhow!(
            "root of {} is not a JSON object",
            claude_json_path.display()
        )
    })?;

    let projects_val = obj
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let projects = projects_val.as_object_mut().ok_or_else(|| {
        anyhow!(
            "'projects' in {} is not a JSON object",
            claude_json_path.display()
        )
    })?;

    let mut changed = false;
    for dir in dirs {
        let key = dir.to_string_lossy().to_string();
        let project_val = projects.entry(key).or_insert_with(|| serde_json::json!({}));
        if let Some(project) = project_val.as_object_mut() {
            if project.get("hasTrustDialogAccepted") != Some(&serde_json::Value::Bool(true)) {
                project.insert(
                    "hasTrustDialogAccepted".to_string(),
                    serde_json::Value::Bool(true),
                );
                changed = true;
            }
        }
    }

    if changed {
        let formatted = serde_json::to_string_pretty(&root)? + "\n";
        crate::hooks::write_if_changed(claude_json_path, &formatted)?;
    }
    Ok(changed)
}

pub fn update_codex_trust(
    codex_config_path: &Path,
    dirs: &[PathBuf],
    create_if_missing: bool,
) -> Result<bool> {
    if !codex_config_path.exists() && !create_if_missing {
        return Ok(false);
    }
    let text = match fs::read_to_string(codex_config_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", codex_config_path.display())),
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("parse TOML from {}", codex_config_path.display()))?;

    let mut changed = false;
    let root = doc.as_table_mut();
    if !matches!(root.get("projects"), Some(item) if item.is_table()) {
        let mut projects_table = Table::new();
        projects_table.set_implicit(true);
        root.insert("projects", Item::Table(projects_table));
        changed = true;
    }
    let projects_table = root
        .get_mut("projects")
        .unwrap()
        .as_table_mut()
        .expect("projects table");

    for dir in dirs {
        let key = dir.to_string_lossy().to_string();
        let project = ensure_child_table(projects_table, &key);
        let is_trusted = project.get("trust_level").and_then(|v| v.as_str()) == Some("trusted");
        if !is_trusted {
            project.insert("trust_level", toml_edit::value("trusted"));
            changed = true;
        }
    }

    if changed {
        crate::hooks::write_if_changed(codex_config_path, &doc.to_string())?;
    }
    Ok(changed)
}

pub fn prepare_workspace_trust_in_roots(
    workspace: &Path,
    cwd: &Path,
    engine: &str,
    env: &BTreeMap<String, String>,
    home: &Path,
) -> Result<()> {
    let mut dirs = Vec::new();
    if let Ok(c_ws) = canonical_workspace(workspace) {
        dirs.push(c_ws);
    } else {
        dirs.push(workspace.to_path_buf());
    }
    if let Ok(c_cwd) = canonical_workspace(cwd) {
        if !dirs.contains(&c_cwd) {
            dirs.push(c_cwd);
        }
    } else if !dirs.contains(&cwd.to_path_buf()) {
        dirs.push(cwd.to_path_buf());
    }

    let family = engine_family(engine);
    let is_shell = engine == "shell";
    let is_claude = family == "claude";
    let is_codex = family == "codex";
    let is_zcodex = engine == "zcodex";

    // 1. Claude trust preparation
    if is_claude || is_shell {
        let claude_json = home.join(".claude.json");
        let create_claude = is_claude;
        if is_claude || claude_json.exists() || home.join(".claude").is_dir() {
            if let Err(error) = update_claude_trust(&claude_json, &dirs, create_claude) {
                eprintln!(
                    "warning: could not update Claude workspace trust in {} ({error:#})",
                    claude_json.display()
                );
            }
        }
        if let Some(claude_dir) = env.get("CLAUDE_CONFIG_DIR") {
            let env_claude_json = Path::new(claude_dir).join(".claude.json");
            if let Err(error) = update_claude_trust(&env_claude_json, &dirs, is_claude) {
                eprintln!(
                    "warning: could not update Claude workspace trust in {} ({error:#})",
                    env_claude_json.display()
                );
            }
        }
    }

    // 2. Codex / zcodex trust preparation
    if is_codex || is_shell {
        let codex_config = home.join(".codex/config.toml");
        let zcodex_config = home.join(".zcodex/config.toml");
        let has_zcodex_dir = home.join(".zcodex").is_dir();
        let has_codex_dir = home.join(".codex").is_dir();

        // Codex config
        let create_codex = is_codex && !is_zcodex;
        if is_codex || codex_config.exists() || has_codex_dir {
            if let Err(error) = update_codex_trust(&codex_config, &dirs, create_codex) {
                eprintln!(
                    "warning: could not update Codex workspace trust in {} ({error:#})",
                    codex_config.display()
                );
            }
        }

        // zcodex config
        let create_zcodex = is_zcodex;
        if is_zcodex || zcodex_config.exists() || has_zcodex_dir {
            if let Err(error) = update_codex_trust(&zcodex_config, &dirs, create_zcodex) {
                eprintln!(
                    "warning: could not update zcodex workspace trust in {} ({error:#})",
                    zcodex_config.display()
                );
            }
        }

        // CODEX_HOME in env
        if let Some(codex_home) = env.get("CODEX_HOME") {
            let env_codex_config = Path::new(codex_home).join("config.toml");
            if let Err(error) = update_codex_trust(&env_codex_config, &dirs, is_codex) {
                eprintln!(
                    "warning: could not update Codex workspace trust in {} ({error:#})",
                    env_codex_config.display()
                );
            }
        }
    }

    Ok(())
}

pub fn prepare_workspace_trust(
    workspace: &Path,
    cwd: &Path,
    engine: &str,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    let home = crate::paths::home_dir()?;
    prepare_workspace_trust_in_roots(workspace, cwd, engine, env, &home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_trust_creation_and_update() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let ws = temp.path().join("worktree-alpha");
        fs::create_dir_all(&ws).unwrap();

        let claude_json = home.join(".claude.json");
        assert!(!claude_json.exists());

        // Prepare trust for claude engine creates .claude.json
        prepare_workspace_trust_in_roots(&ws, &ws, "claude", &BTreeMap::new(), home).unwrap();
        assert!(claude_json.exists());

        let content = fs::read_to_string(&claude_json).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        let ws_key = crate::canonical_workspace(&ws)
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            val["projects"][&ws_key]["hasTrustDialogAccepted"],
            serde_json::Value::Bool(true)
        );

        // Idempotence: calling again changes nothing
        let wrote = update_claude_trust(&claude_json, std::slice::from_ref(&ws), false).unwrap();
        assert!(!wrote);
    }

    #[test]
    fn test_codex_trust_creation_and_update() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let ws = temp.path().join("worktree-beta");
        fs::create_dir_all(&ws).unwrap();

        let codex_config = home.join(".codex/config.toml");
        assert!(!codex_config.exists());

        // Prepare trust for codex engine creates .codex/config.toml
        prepare_workspace_trust_in_roots(&ws, &ws, "codex", &BTreeMap::new(), home).unwrap();
        assert!(codex_config.exists());

        let content = fs::read_to_string(&codex_config).unwrap();
        assert!(content.contains("trust_level = \"trusted\""));
        let doc: DocumentMut = content.parse().unwrap();
        let ws_key = crate::canonical_workspace(&ws)
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            doc["projects"][&ws_key]["trust_level"].as_str(),
            Some("trusted")
        );

        // Idempotence
        let wrote = update_codex_trust(&codex_config, std::slice::from_ref(&ws), false).unwrap();
        assert!(!wrote);
    }

    #[test]
    fn test_zcodex_trust_creation() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let ws = temp.path().join("worktree-gamma");
        fs::create_dir_all(&ws).unwrap();

        let zcodex_config = home.join(".zcodex/config.toml");
        assert!(!zcodex_config.exists());

        prepare_workspace_trust_in_roots(&ws, &ws, "zcodex", &BTreeMap::new(), home).unwrap();
        assert!(zcodex_config.exists());

        let content = fs::read_to_string(&zcodex_config).unwrap();
        assert!(content.contains("trust_level = \"trusted\""));
    }

    #[test]
    fn test_shell_trust_updates_existing_configs() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let ws = temp.path().join("worktree-delta");
        fs::create_dir_all(&ws).unwrap();

        // Create empty .claude.json and .codex/config.toml
        let claude_json = home.join(".claude.json");
        fs::write(&claude_json, "{\"existing\": 123}").unwrap();
        let codex_dir = home.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let codex_config = codex_dir.join("config.toml");
        fs::write(&codex_config, "model = \"gpt-4o\"\n").unwrap();

        // Prepare trust with shell engine
        prepare_workspace_trust_in_roots(&ws, &ws, "shell", &BTreeMap::new(), home).unwrap();

        // Claude file has both existing field and projects
        let c_content = fs::read_to_string(&claude_json).unwrap();
        let c_val: serde_json::Value = serde_json::from_str(&c_content).unwrap();
        assert_eq!(c_val["existing"], 123);
        let ws_key = crate::canonical_workspace(&ws)
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            c_val["projects"][&ws_key]["hasTrustDialogAccepted"],
            serde_json::Value::Bool(true)
        );

        // Codex file preserved model and added projects
        let toml_content = fs::read_to_string(&codex_config).unwrap();
        assert!(toml_content.contains("model = \"gpt-4o\""));
        assert!(toml_content.contains("trust_level = \"trusted\""));
    }
}
