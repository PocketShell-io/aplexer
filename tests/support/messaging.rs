use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
use uuid::Uuid;

pub(crate) struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    workspace: TempDir,
}

impl Harness {
    pub(crate) fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = state.path().join("config.toml");
        let workspace = TempDir::new().unwrap();
        Self {
            runtime,
            state,
            config,
            workspace,
        }
    }

    pub(crate) fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        }
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aplexer"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config);
        command
    }

    pub(crate) fn record(
        &self,
        phase: Phase,
        worker_pid: Option<u32>,
        history: &[u8],
    ) -> SessionRecord {
        self.record_in(self.workspace.path(), phase, worker_pid, history)
    }

    pub(crate) fn record_in(
        &self,
        workspace: &Path,
        phase: Phase,
        worker_pid: Option<u32>,
        history: &[u8],
    ) -> SessionRecord {
        let paths = self.paths();
        paths.ensure().unwrap();
        let id = Uuid::now_v7();
        let record = SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: workspace.to_path_buf(),
            tag: format!("capture-{id}"),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/sh".into()],
            cwd: workspace.to_path_buf(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: 4096,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            wake: None,
            agent_override: None,
            phase,
            worker_pid,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: None,
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        };
        std::fs::create_dir_all(paths.state_session(id)).unwrap();
        std::fs::write(&record.history_path, history).unwrap();
        atomic_write_json(&paths.record(id), &record).unwrap();
        record
    }
}
