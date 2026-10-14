//! PocketShell schema-3 mapping and pickSession ranking (opt-in wire mode).
use super::*;

pub(super) fn live(row: &Value) -> bool {
    matches!(
        row["state"].as_str(),
        Some("starting" | "running" | "exiting")
    )
}
pub(super) fn name(record: &SessionRecord) -> String {
    match record
        .workspace
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
    {
        Some(base) if !record.tag.is_empty() => format!("{base}:{}", record.tag.trim()),
        _ if !record.tag.is_empty() => record.tag.trim().to_owned(),
        _ => record.id.to_string(),
    }
}
pub(super) fn row(record: &SessionRecord, raw: &Value) -> Value {
    let now = now_ms();
    let (state, source) = if matches!(record.phase, Phase::Exited | Phase::Failed) {
        (Value::Null, Value::Null)
    } else if record.phase == Phase::Starting {
        (json!("working"), json!("heuristic"))
    } else if record
        .reported_state
        .as_deref()
        .is_some_and(|s| matches!(s, "idle" | "waiting" | "working"))
        && record
            .reported_state_at_ms
            .is_some_and(|t| t > 0 && now.saturating_sub(t) <= 8000)
    {
        (json!(record.reported_state), json!("reported"))
    } else if record
        .last_activity_ms
        .is_none_or(|t| t == 0 || now.saturating_sub(t) < 3000)
    {
        (json!("working"), json!("heuristic"))
    } else {
        (json!("waiting"), json!("heuristic"))
    };
    let epoch = |ms: Option<u64>| ms.filter(|t| *t > 0).map(|t| t / 1000);
    let attached = raw["attached_clients"].as_u64().is_some_and(|n| n > 0)
        || raw["attached_clients"].as_bool().unwrap_or(false)
        || raw["attached"].as_bool().unwrap_or(false)
        || raw["attached"].as_u64().is_some_and(|n| n > 0);
    json!({"name":name(record),"id":record.id,"workspace":record.workspace,"tag":record.tag,
        "engine":if record.engine=="shell" {None} else {Some(&record.engine)},"profile":record.profile,
        "agent":raw["agent"],"agent_state":state,"agent_state_source":source,"attached":attached,
        "created_epoch":epoch(Some(record.created_at_ms)),"activity_epoch":epoch(record.last_activity_ms),
        "phase":record.phase,"alive":live(raw)})
}

// Exact name/ID first (first row wins, as in hostClient.ts); then union of
// exact tag and >=4-character case-sensitive UUID prefix, deduplicated by ID.
pub(super) fn resolve(
    records: &[SessionRecord],
    rows: &Value,
    selector: &str,
) -> std::result::Result<SessionRecord, Value> {
    let records: Vec<_> = records
        .iter()
        .filter(|r| {
            rows.as_array()
                .unwrap()
                .iter()
                .any(|v| v["id"] == r.id.to_string() && live(v))
        })
        .collect();
    if let Some(record) = records
        .iter()
        .find(|r| name(r) == selector || r.id.to_string() == selector)
    {
        return Ok((*record).clone());
    }
    let mut matches: Vec<_> = records
        .iter()
        .filter(|r| r.tag == selector)
        .copied()
        .collect();
    if selector.len() >= 4 {
        for record in &records {
            if record.id.to_string().starts_with(selector)
                && !matches.iter().any(|r| r.id == record.id)
            {
                matches.push(record);
            }
        }
    }
    match matches.as_slice() {
        [record] => Ok((*record).clone()),
        [] => Err(
            json!({"code":"SESSION_NOT_FOUND","message":format!("no session matches {} (see `sessions list`)",json!(selector)),"notDispatched":true}),
        ),
        _ => {
            let candidates: Vec<_> = matches
                .iter()
                .map(|r| json!({"name":name(r),"id":r.id}))
                .collect();
            let names: Vec<_> = matches.iter().map(|r| name(r)).collect();
            Err(
                json!({"code":"SESSION_AMBIGUOUS","message":format!("{} matches {} sessions: {} — use the full name or id",json!(selector),matches.len(),names.join(", ")),"notDispatched":true,"details":{"candidates":candidates}}),
            )
        }
    }
}
