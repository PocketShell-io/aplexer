//! Incremental resident registry, reconciliation and derived-data sampling.
use super::*;

type ListBytes = Arc<Vec<u8>>;

pub(super) struct Snapshot {
    pub(super) records: Vec<SessionRecord>,
    pub(super) rows: Value,
    pub(super) list_bytes: Arc<Vec<u8>>,
    pub(super) psc_bytes: Arc<Vec<u8>>,
    usage: BTreeMap<Uuid, SessionProcUsage>,
    usage_refreshed: Instant,
    usage_sampled: Instant,
    refreshed: Instant,
    generation: u64,
    error: Option<String>,
}

impl Snapshot {
    pub(super) fn load(paths: &Paths) -> Result<Self> {
        aplexer::warnings::sweep_warnings(paths);
        let records = list_records(paths)?;
        let rows = api::snapshot_records_json(paths, &records, false)?;
        let (list_bytes, psc_bytes) = encoded_lists(&records, &rows)?;
        let usage = rows
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| {
                let id = row["id"].as_str()?.parse().ok()?;
                Some((
                    id,
                    SessionProcUsage {
                        processes: row["processes"].as_u64().unwrap_or(0) as u32,
                        cpu_percent: row["cpu_percent"].as_f64(),
                    },
                ))
            })
            .collect();
        Ok(Self {
            records,
            rows,
            list_bytes,
            psc_bytes,
            usage,
            usage_refreshed: Instant::now(),
            usage_sampled: Instant::now(),
            refreshed: Instant::now(),
            generation: 1,
            error: None,
        })
    }
    pub(super) fn unavailable(&self) -> Option<String> {
        self.error.clone().or_else(|| {
            (self.refreshed.elapsed() > Duration::from_secs(2))
                .then(|| "registry snapshot exceeds 2 second freshness bound".to_owned())
        })
    }
    pub(super) fn meta(&self) -> Value {
        json!({"generation":self.generation,"ageMs":self.refreshed.elapsed().as_millis(),"processAgeMs":self.usage_refreshed.elapsed().as_millis()})
    }
}

fn encoded_lists(records: &[SessionRecord], rows: &Value) -> Result<(ListBytes, ListBytes)> {
    let sessions: Vec<_> = records
        .iter()
        .filter_map(|r| {
            let raw = rows
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["id"] == r.id.to_string())?;
            psc::live(raw).then(|| psc::row(r, raw))
        })
        .collect();
    Ok((
        Arc::new(serde_json::to_vec(rows)?),
        Arc::new(serde_json::to_vec(
            &json!({"schema":3,"sessions":sessions,"errors":[]}),
        )?),
    ))
}

pub(super) fn usage_loop(paths: Paths, state: Arc<RwLock<Snapshot>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let records = state.read().unwrap().records.clone();
        let roots = live_worker_roots(records.iter());
        let usage =
            cached_session_proc_usage(Path::new("/proc"), &paths.proc_usage_cache(), &roots);
        {
            let mut guard = state.write().unwrap();
            guard.usage = usage;
            guard.usage_sampled = Instant::now();
        }
        for _ in 0..25 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

// Metadata polling is incremental (only changed records are reparsed). Every
// second a full strict scan reconciles missed changes and corrupt entries.
pub(super) fn refresh_loop(paths: Paths, state: Arc<RwLock<Snapshot>>, stop: Arc<AtomicBool>) {
    let mut signatures = HashMap::new();
    let mut reconciled = Instant::now() - Duration::from_secs(2);
    let mut enriched = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(20));
        let result = (|| -> Result<()> {
            let mut records = state.read().unwrap().records.clone();
            let mut changed = false;
            if reconciled.elapsed() >= Duration::from_secs(1) {
                records = list_records(&paths)?;
                reconciled = Instant::now();
                changed = true;
            }
            let mut next = HashMap::new();
            for record in &mut records {
                match fs::metadata(paths.record(record.id)) {
                    Ok(metadata) => {
                        let signature = (metadata.modified()?, metadata.len());
                        if signatures.get(&record.id) != Some(&signature) {
                            *record = read_session_record(&paths, record.id)?;
                            changed = true;
                        }
                        next.insert(record.id, signature);
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        changed = true;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            records.retain(|r| next.contains_key(&r.id));
            // Root directory metadata changes identify creation/deletion without
            // reopening every record. Full reconciliation handles missed events.
            let root = fs::metadata(paths.state_root.join("sessions"))?.modified()?;
            let root_id = Uuid::nil();
            if signatures.get(&root_id) != Some(&(root, 0)) {
                records = list_records(&paths)?;
                changed = true;
            }
            next.insert(root_id, (root, 0));
            signatures = next;
            if changed || enriched.elapsed() >= Duration::from_millis(50) {
                for record in &records {
                    aplexer::warnings::record_warning_best_effort(&paths, record);
                }
                let (usage, usage_sampled) = {
                    let guard = state.read().unwrap();
                    (guard.usage.clone(), guard.usage_sampled)
                };
                let rows =
                    api::snapshot_records_json_with_usage(&paths, &records, false, Some(&usage))?;
                let (list_bytes, psc_bytes) = encoded_lists(&records, &rows)?;
                let mut guard = state.write().unwrap();
                guard.records = records;
                guard.rows = rows;
                guard.usage_refreshed = usage_sampled;
                guard.list_bytes = list_bytes;
                guard.psc_bytes = psc_bytes;
                guard.refreshed = Instant::now();
                guard.generation += 1;
                guard.error = None;
                enriched = Instant::now();
            }
            Ok(())
        })();
        if let Err(e) = result {
            state.write().unwrap().error = Some(format!("{e:#}"));
        }
    }
}
