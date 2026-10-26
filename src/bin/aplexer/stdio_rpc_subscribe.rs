//! PSC-RPC-2 push subscriptions: sessions snapshots and screen mirrors.
//!
//! Protocol 2 is a strict superset of protocol 1 served by the same helper
//! process. The request loop answers `subscribe`/`unsubscribe` synchronously
//! (like the cached `list` fast path) so the response always precedes the
//! first event on the wire. A dedicated publisher thread evaluates
//! subscriptions on a 20 ms tick and a writer thread drains the bounded
//! event queue to stdout; neither ever holds a lock across blocking I/O.
use super::*;
use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

/// At most one sessions snapshot per subscription per 50 ms (coalesced).
const SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(50);
/// Screen mirrors push at most 10 Hz and only on content change.
const SCREEN_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Screen topics are polled at the same cadence they may emit at.
const SCREEN_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// A topic that produced no frame for this long gets a heartbeat.
const HEARTBEAT_IDLE: Duration = Duration::from_secs(5);
/// A topic whose screen capture failed is skipped this long (a wedged
/// worker must not stall snapshots and heartbeats on the publisher tick).
const CAPTURE_BACKOFF: Duration = Duration::from_secs(2);
/// Publisher tick: well under the 200 ms change-to-snapshot bound.
const PUBLISH_TICK: Duration = Duration::from_millis(20);
/// Bounded event queue; overflow drops the oldest and marks the gap.
const EVENT_QUEUE_CAP: usize = 16;
/// At most this many screen topics per helper generation.
const MAX_SCREEN_TOPICS: usize = 16;

/// Raw-list row fields that tick with wall-clock or sampler activity rather
/// than session events. `state`/`phase`/`exit`/`reported_state` stay: their
/// flips (lifecycle, crash, genuine agent reports) must push within 200 ms.
const RAW_VOLATILE_FIELDS: &[&str] = &[
    "cpu_percent",
    "processes",
    "last_activity_ms",
    "last_accessed_ms",
    "updated_at_ms",
    "reported_state_at_ms",
    "reported_state_event_ms",
];
/// Schema-3 row fields with the same property. `agent_state` covers both
/// the reported states and the wall-clock activity heuristic; genuine
/// reported transitions ride along with activity and arrive within 5 s.
const PSC_VOLATILE_FIELDS: &[&str] = &["activity_epoch", "agent_state", "agent_state_source"];

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Activity-stripped hash of a sessions snapshot: wall-clock and sampler
/// fields are excluded, so flowing output or process sampling alone never
/// counts as a change. Everything event-like (membership, phase, state,
/// tags, agent, attachment, warnings, errors) stays verbatim and still
/// pushes within 200 ms.
fn stable_sessions_key(data: &Value, format_psc: bool) -> u64 {
    fn row_key(row: &Value, volatile: &[&str]) -> String {
        match row.as_object() {
            Some(map) => {
                let mut parts: Vec<String> = map
                    .iter()
                    .filter(|(key, _)| !volatile.contains(&key.as_str()))
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect();
                parts.sort();
                parts.join(",")
            }
            None => row.to_string(),
        }
    }
    if format_psc {
        let mut rows: Vec<String> = data
            .get("sessions")
            .and_then(Value::as_array)
            .map(|sessions| {
                sessions
                    .iter()
                    .map(|session| row_key(session, PSC_VOLATILE_FIELDS))
                    .collect()
            })
            .unwrap_or_default();
        rows.sort();
        let errors = data.get("errors").map(Value::to_string).unwrap_or_default();
        content_hash(format!("{}|{errors}", rows.join(";")).as_bytes())
    } else {
        let mut rows: Vec<String> = data
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| row_key(row, RAW_VOLATILE_FIELDS))
                    .collect()
            })
            .unwrap_or_default();
        rows.sort();
        content_hash(rows.join(";").as_bytes())
    }
}

pub(super) enum Topic {
    Sessions,
    Screen {
        topic: String,
        selector: Uuid,
        psc_mode: bool,
    },
}

impl Topic {
    fn name(&self) -> &str {
        match self {
            Topic::Sessions => "sessions",
            Topic::Screen { topic, .. } => topic,
        }
    }
}

pub(super) struct TopicState {
    topic: Topic,
    /// Last emission time (coalesce/throttle gate).
    emitted: Instant,
    /// Last frame time of any kind (heartbeat deadline).
    framed: Instant,
    /// Full content hash at the last emission.
    hash: Option<u64>,
    /// Activity-stripped hash at the last emission (sessions topics).
    stable: Option<u64>,
    /// Last screen poll time.
    polled: Instant,
    /// A failed screen capture backs the topic off until this time.
    backoff_until: Instant,
    /// The request loop sets this after writing the initial frames, so the
    /// publisher never emits for a subscription whose response is not yet
    /// on the wire (wire order would otherwise invert under a concurrent tick).
    primed: bool,
}

pub(super) struct Subscription {
    topics: Vec<TopicState>,
    /// Sessions encoding for this subscription.
    format_psc: bool,
}

#[derive(Default)]
pub(super) struct Subs {
    next: u64,
    subscriptions: HashMap<String, Subscription>,
}

impl Subs {
    pub(super) fn screen_topics(&self) -> usize {
        self.subscriptions
            .values()
            .flat_map(|s| &s.topics)
            .filter(|t| matches!(t.topic, Topic::Screen { .. }))
            .count()
    }

    /// Mark every topic of a subscription live after its initial frames are
    /// on the wire. See `prepare_subscribe` for the ordering argument.
    pub(super) fn prime(&mut self, id: &str) {
        if let Some(sub) = self.subscriptions.get_mut(id) {
            for topic in &mut sub.topics {
                topic.primed = true;
            }
        }
    }

    pub(super) fn insert(&mut self, id: String, topics: Vec<TopicState>, format_psc: bool) {
        self.subscriptions
            .insert(id, Subscription { topics, format_psc });
    }
}

/// Event kinds in replace precedence order: a queued frame is replaced in
/// place only by a frame of equal-or-higher information content, so a
/// heartbeat never clobbers a queued snapshot and an `ended` always wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    Heartbeat,
    Screen,
    Snapshot,
    Ended,
}

struct Queued {
    sub: String,
    topic: String,
    kind: Kind,
    frame: Value,
}

struct BusInner {
    queue: VecDeque<Queued>,
    /// Keys whose queued frame was evicted under pressure: their next frame
    /// carries `gap:true`.
    gap_keys: HashSet<(String, String)>,
}

pub(super) struct Bus {
    inner: Mutex<BusInner>,
    wake: Condvar,
    seq: AtomicU64,
}

impl Bus {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(BusInner {
                queue: VecDeque::new(),
                gap_keys: HashSet::new(),
            }),
            wake: Condvar::new(),
            seq: AtomicU64::new(1),
        }
    }

    pub(super) fn alloc_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Latest wins per (subscription, topic): a newer frame for a key with a
    /// queued frame replaces it in place and carries `gap:true` (an
    /// intermediate state was skipped). `ended` additionally evicts any
    /// queued frame of its topic and is itself never dropped. A heartbeat
    /// newer than queued state is dropped outright: the queued frame proves
    /// freshness better. Overflow evicts the oldest non-`ended` frame and
    /// marks its key, so the gap always lands on the right topic.
    pub(super) fn push(&self, sub: &str, topic: &str, kind: Kind, mut frame: Value) {
        let mut guard = self.inner.lock().unwrap();
        if kind == Kind::Ended {
            // The terminal truth supersedes any undelivered frame of its
            // topic; like every replacement, it carries the gap when it
            // actually displaces one.
            let displaced = guard.queue.iter().any(|q| q.sub == sub && q.topic == topic);
            guard.queue.retain(|q| !(q.sub == sub && q.topic == topic));
            if displaced {
                if let Some(object) = frame.as_object_mut() {
                    object.insert("gap".to_owned(), Value::Bool(true));
                }
            }
        } else if let Some(existing) = guard
            .queue
            .iter_mut()
            .find(|q| q.sub == sub && q.topic == topic)
        {
            if kind >= existing.kind {
                existing.kind = kind;
                existing.frame = frame;
                if let Some(object) = existing.frame.as_object_mut() {
                    object.insert("gap".to_owned(), Value::Bool(true));
                }
                guard.gap_keys.remove(&(sub.to_owned(), topic.to_owned()));
            }
            self.wake.notify_one();
            return;
        }
        while guard.queue.len() >= EVENT_QUEUE_CAP {
            let evicted = guard.queue.iter().position(|q| q.kind != Kind::Ended);
            match evicted {
                Some(index) => {
                    let evicted = guard.queue.remove(index).unwrap();
                    guard.gap_keys.insert((evicted.sub, evicted.topic));
                }
                None => break,
            }
        }
        let key = (sub.to_owned(), topic.to_owned());
        if guard.gap_keys.remove(&key) {
            if let Some(object) = frame.as_object_mut() {
                object.insert("gap".to_owned(), Value::Bool(true));
            }
        }
        guard.queue.push_back(Queued {
            sub: key.0,
            topic: key.1,
            kind,
            frame,
        });
        self.wake.notify_one();
    }

    pub(super) fn notify(&self) {
        self.wake.notify_one();
    }
}

/// Extra parameter rules for the protocol-2 control methods. The shared
/// shape rules (object params, `format`/`selectorMode` values) live in
/// `operations::validate`, which only reaches this for protocol 2.
pub(super) fn validate(call: &Call) -> Result<()> {
    match call.method.as_str() {
        "subscribe" => {
            let topics = call.params["topics"]
                .as_array()
                .ok_or_else(|| anyhow!("topics must be a non-empty array of strings"))?;
            if topics.is_empty() || topics.iter().any(|t| t.as_str().is_none_or(str::is_empty)) {
                bail!("topics must be a non-empty array of strings");
            }
            let mut seen = HashSet::new();
            for topic in topics.iter().map(|t| t.as_str().unwrap()) {
                if !seen.insert(topic) {
                    bail!("duplicate topic {topic:?}");
                }
                if topic != "sessions"
                    && !(topic.starts_with("screen:") && topic.len() > "screen:".len())
                {
                    bail!("unsupported topic {topic:?}");
                }
            }
        }
        "unsubscribe" => {
            if call.params["subscription"]
                .as_str()
                .is_none_or(str::is_empty)
            {
                bail!("subscription must be a non-empty string");
            }
        }
        _ => bail!("unsupported method"),
    }
    Ok(())
}

fn event_frame(
    proto: u32,
    event: &str,
    subscription: &str,
    topic: &str,
    seq: u64,
    data: Option<Value>,
    generation: Option<&str>,
) -> Value {
    let mut frame = serde_json::Map::new();
    frame.insert("protocol".to_owned(), json!(proto));
    frame.insert("event".to_owned(), json!(event));
    frame.insert("subscription".to_owned(), json!(subscription));
    frame.insert("topic".to_owned(), json!(topic));
    frame.insert("seq".to_owned(), json!(seq));
    if let Some(data) = data {
        frame.insert("data".to_owned(), data);
    }
    if let Some(generation) = generation {
        frame.insert("generation".to_owned(), json!(generation));
    }
    Value::Object(frame)
}

fn snapshot_value(bytes: &[u8]) -> Result<Value> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Resolve one screen topic selector with the same rules as `capture`.
/// Returns the pinned record or the complete error frame for the request.
fn resolve_screen(
    proto: u32,
    id: &Value,
    records: &[SessionRecord],
    rows: &Value,
    selector: &str,
    psc_mode: bool,
) -> std::result::Result<SessionRecord, Value> {
    if psc_mode {
        psc::resolve(records, rows, selector)
            .map_err(|error| json!({"protocol":proto,"id":id,"error":error}))
    } else {
        resolve_records(records, Some(selector), None, None).map_err(|e| {
            let code = if e.to_string() == NO_MATCHING_SESSION {
                "not_found"
            } else {
                "ambiguous"
            };
            failure(id.clone(), proto, code, e, true)
        })
    }
}

/// Screen result matching a `capture screenPlain` call, for event payloads.
/// Like the request/response path, a dead worker falls back to the retained
/// `screen.txt` instead of failing the mirror.
fn screen_data(
    paths: &Paths,
    record: &SessionRecord,
    rows: &Value,
    psc_mode: bool,
) -> Result<(Vec<u8>, Value)> {
    let bytes = match rpc_capture_screen(record, true) {
        Ok(bytes) => bytes,
        Err(_) if record.worker_finished() || !record.worker_alive() => {
            fs::read(paths.screen_txt(record.id))?
        }
        Err(e) => return Err(e),
    };
    if bytes.len() > PAYLOAD_LIMIT {
        bail!("screen exceeds the per-frame byte budget");
    }
    let rerecord = read_session_record(paths, record.id)?;
    let mut result = capture_json_value(&rerecord, &bytes);
    if psc_mode {
        let row = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"].as_str() == Some(&rerecord.id.to_string()))
            .cloned()
            .unwrap_or(Value::Null);
        result["session"] = psc::row(&rerecord, &row);
        result["text"] = result["utf8"].clone();
    }
    Ok((bytes, result))
}

fn session_live(record: &SessionRecord) -> bool {
    !matches!(record.phase, Phase::Exited | Phase::Failed)
}

/// Prepared subscription: everything `subscribe` needs before it takes the
/// stdout lock. Resolution and the initial screen captures run here, lock
/// free, so a wedged worker stalls neither the event writer nor other
/// responses waiting on stdout.
pub(super) struct Prepared {
    pub(super) sub_id: String,
    pub(super) result: Value,
    pub(super) initial: Vec<Value>,
    pub(super) states: Vec<TopicState>,
    pub(super) format_psc: bool,
}

/// Prepare `subscribe` without touching stdout or inserting anything, so
/// the request loop can capture first and lock afterwards. Returns the
/// prepared subscription for the loop to insert, write and prime.
pub(super) fn prepare_subscribe(
    proto: u32,
    call: &Call,
    paths: &Paths,
    state: &Arc<RwLock<Snapshot>>,
    subs: &Arc<Mutex<Subs>>,
    bus: &Arc<Bus>,
) -> std::result::Result<Prepared, Value> {
    let request_id = json!(call.id.clone());
    let fail = |code: &str, message: String| -> Value {
        failure(request_id.clone(), proto, code, message, true)
    };
    let format_psc = call.params["format"] == "psc";
    let psc_mode = call.params["selectorMode"] == "psc";
    let topics: Vec<&str> = call.params["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    let guard = state.read().unwrap();
    if let Some(e) = guard.unavailable() {
        return Err(fail("registry_unavailable", e));
    }
    // Clone out from under the read lock; resolution and the initial screen
    // captures below do blocking worker I/O and must not stall the refresh
    // loop's write lock.
    let records = guard.records.clone();
    let rows = guard.rows.clone();
    let list_bytes: Arc<Vec<u8>> = if format_psc {
        guard.psc_bytes.clone()
    } else {
        guard.list_bytes.clone()
    };
    drop(guard);
    let new_screens = topics.iter().filter(|t| t.starts_with("screen:")).count();
    let sub_id = {
        let mut subs_guard = subs.lock().unwrap();
        if subs_guard.screen_topics() + new_screens > MAX_SCREEN_TOPICS {
            return Err(fail(
                "invalid_request",
                format!("at most {MAX_SCREEN_TOPICS} screen topics per helper"),
            ));
        }
        subs_guard.next += 1;
        format!("sub-{}", subs_guard.next)
    };
    let mut resolved = Vec::new();
    for topic in &topics {
        if let Some(selector) = topic.strip_prefix("screen:") {
            match resolve_screen(proto, &request_id, &records, &rows, selector, psc_mode) {
                Ok(record) => resolved.push(((*topic).to_owned(), record.id)),
                Err(frame) => return Err(frame),
            }
        }
    }
    let now = Instant::now();
    let mut initial = Vec::new();
    let mut states = Vec::new();
    let mut first_seq: Option<u64> = None;
    if topics.contains(&"sessions") {
        let data: Value =
            snapshot_value(&list_bytes).map_err(|e| fail("operation_failed", format!("{e:#}")))?;
        let seq = bus.alloc_seq();
        first_seq.get_or_insert(seq);
        initial.push(event_frame(
            proto,
            "snapshot",
            &sub_id,
            "sessions",
            seq,
            Some(data.clone()),
            None,
        ));
        states.push(TopicState {
            topic: Topic::Sessions,
            emitted: now,
            framed: now,
            hash: Some(content_hash(&list_bytes)),
            stable: Some(stable_sessions_key(&data, format_psc)),
            polled: now,
            backoff_until: now,
            primed: false,
        });
    }
    for (topic, uuid) in resolved {
        let mut baseline = None;
        if let Ok(fresh) = read_session_record(paths, uuid) {
            if session_live(&fresh) {
                if let Ok((bytes, data)) = screen_data(paths, &fresh, &rows, psc_mode) {
                    let seq = bus.alloc_seq();
                    first_seq.get_or_insert(seq);
                    initial.push(event_frame(
                        proto,
                        "screen",
                        &sub_id,
                        &topic,
                        seq,
                        Some(data),
                        None,
                    ));
                    baseline = Some(content_hash(&bytes));
                }
            }
        }
        states.push(TopicState {
            topic: Topic::Screen {
                topic,
                selector: uuid,
                psc_mode,
            },
            emitted: now,
            framed: now,
            hash: baseline,
            stable: None,
            polled: now,
            // A failed first capture backs off like a publisher miss, so a
            // wedged worker does not hot-poll from the first tick.
            backoff_until: if baseline.is_none() {
                now + CAPTURE_BACKOFF
            } else {
                now
            },
            primed: false,
        });
    }
    let seq = first_seq.unwrap_or_else(|| bus.seq.load(Ordering::Relaxed));
    Ok(Prepared {
        sub_id: sub_id.clone(),
        result: json!({"subscription":sub_id,"seq":seq}),
        initial,
        states,
        format_psc,
    })
}

/// Handle `unsubscribe` synchronously; it never touches the registry.
pub(super) fn handle_unsubscribe(
    proto: u32,
    call: &Call,
    subs: &Arc<Mutex<Subs>>,
) -> std::result::Result<Value, Value> {
    let id = call.params["subscription"].as_str().unwrap().to_owned();
    let mut guard = subs.lock().unwrap();
    if guard.subscriptions.remove(&id).is_none() {
        return Err(failure(
            json!(call.id.clone()),
            proto,
            "invalid_request",
            format!("unknown subscription {id:?}"),
            true,
        ));
    }
    Ok(json!({"unsubscribed":id}))
}

struct ScreenPoll {
    sub: String,
    index: usize,
    name: String,
    uuid: Uuid,
    psc_mode: bool,
}

enum ScreenOutcome {
    Captured(u64, Value),
    Ended,
    Skipped,
}

/// Capture one screen topic outside any lock. A missing record or a
/// terminal phase ends the topic; any other capture failure skips this tick
/// without moving the baseline, so a wedged worker never ends a live topic.
fn capture_screen_topic(paths: &Paths, rows: &Value, poll: &ScreenPoll) -> ScreenOutcome {
    let record = match read_session_record(paths, poll.uuid) {
        Ok(record) => record,
        Err(_) => return ScreenOutcome::Ended,
    };
    if !session_live(&record) {
        return ScreenOutcome::Ended;
    }
    match screen_data(paths, &record, rows, poll.psc_mode) {
        Ok((bytes, data)) => ScreenOutcome::Captured(content_hash(&bytes), data),
        Err(_) => ScreenOutcome::Skipped,
    }
}

/// Evaluate every primed topic: sessions snapshots (coalesced), screen
/// mirrors (10 Hz, on change), and heartbeat deadlines. Blocking captures
/// run outside the locks; emission re-validates under them, so an
/// unsubscribe racing a capture never emits for a dead subscription.
pub(super) fn publisher_loop(
    proto: u32,
    paths: Paths,
    state: Arc<RwLock<Snapshot>>,
    subs: Arc<Mutex<Subs>>,
    bus: Arc<Bus>,
    info: Arc<HelperInfo>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(PUBLISH_TICK);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let (polls, rows) = {
            let mut subs_guard = subs.lock().unwrap();
            let guard = state.read().unwrap();
            if guard.unavailable().is_some() {
                continue;
            }
            let mut polls = Vec::new();
            for (id, sub) in subs_guard.subscriptions.iter_mut() {
                for (index, topic) in sub.topics.iter_mut().enumerate() {
                    if !topic.primed {
                        continue;
                    }
                    if let Topic::Screen {
                        topic: name,
                        selector,
                        psc_mode,
                    } = &topic.topic
                    {
                        if now >= topic.backoff_until
                            && now.duration_since(topic.polled) >= SCREEN_POLL_INTERVAL
                        {
                            topic.polled = now;
                            polls.push(ScreenPoll {
                                sub: id.clone(),
                                index,
                                name: name.clone(),
                                uuid: *selector,
                                psc_mode: *psc_mode,
                            });
                        }
                    }
                }
            }
            (polls, guard.rows.clone())
        };
        let outcomes: Vec<ScreenOutcome> = polls
            .iter()
            .map(|poll| capture_screen_topic(&paths, &rows, poll))
            .collect();
        {
            let mut subs_guard = subs.lock().unwrap();
            let guard = state.read().unwrap();
            if guard.unavailable().is_some() {
                continue;
            }
            for (id, sub) in subs_guard.subscriptions.iter_mut() {
                if sub.topics.iter().all(|t| !t.primed) {
                    continue;
                }
                if sub
                    .topics
                    .iter()
                    .any(|t| matches!(t.topic, Topic::Sessions))
                {
                    let bytes: Arc<Vec<u8>> = if sub.format_psc {
                        guard.psc_bytes.clone()
                    } else {
                        guard.list_bytes.clone()
                    };
                    let full = content_hash(&bytes);
                    // Activity-only changes (flowing output, sampler ticks)
                    // ride the heartbeat cadence; anything structural pushes
                    // at the coalescing cadence.
                    let stable = snapshot_value(&bytes)
                        .map(|data| stable_sessions_key(&data, sub.format_psc))
                        .ok();
                    if let Some(topic) = sub
                        .topics
                        .iter_mut()
                        .find(|t| matches!(t.topic, Topic::Sessions))
                    {
                        let structural = stable != topic.stable;
                        if topic.primed
                            && structural
                            && now.duration_since(topic.emitted) >= SNAPSHOT_MIN_INTERVAL
                        {
                            if let Ok(data) = snapshot_value(&bytes) {
                                topic.hash = Some(full);
                                topic.stable = stable;
                                topic.emitted = now;
                                topic.framed = now;
                                bus.push(
                                    id,
                                    "sessions",
                                    Kind::Snapshot,
                                    event_frame(
                                        proto,
                                        "snapshot",
                                        id,
                                        "sessions",
                                        bus.alloc_seq(),
                                        Some(data),
                                        None,
                                    ),
                                );
                            }
                        } else if topic.primed
                            && !structural
                            && topic.hash != Some(full)
                            && now.duration_since(topic.framed) >= HEARTBEAT_IDLE
                        {
                            // Activity moved but nothing structural did: the
                            // snapshot goes out in place of the heartbeat.
                            if let Ok(data) = snapshot_value(&bytes) {
                                topic.hash = Some(full);
                                topic.stable = stable;
                                topic.emitted = now;
                                topic.framed = now;
                                bus.push(
                                    id,
                                    "sessions",
                                    Kind::Snapshot,
                                    event_frame(
                                        proto,
                                        "snapshot",
                                        id,
                                        "sessions",
                                        bus.alloc_seq(),
                                        Some(data),
                                        None,
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            for (poll, outcome) in polls.iter().zip(outcomes) {
                let Some(sub) = subs_guard.subscriptions.get_mut(&poll.sub) else {
                    continue;
                };
                let Some(topic) = sub.topics.get_mut(poll.index) else {
                    continue;
                };
                if topic.topic.name() != poll.name {
                    continue;
                }
                match outcome {
                    ScreenOutcome::Ended => {
                        bus.push(
                            &poll.sub,
                            &poll.name,
                            Kind::Ended,
                            event_frame(
                                proto,
                                "ended",
                                &poll.sub,
                                &poll.name,
                                bus.alloc_seq(),
                                None,
                                None,
                            ),
                        );
                        sub.topics.remove(poll.index);
                    }
                    ScreenOutcome::Captured(hash, data) => {
                        if topic.hash != Some(hash)
                            && now.duration_since(topic.emitted) >= SCREEN_MIN_INTERVAL
                        {
                            topic.hash = Some(hash);
                            topic.emitted = now;
                            topic.framed = now;
                            bus.push(
                                &poll.sub,
                                &poll.name,
                                Kind::Screen,
                                event_frame(
                                    proto,
                                    "screen",
                                    &poll.sub,
                                    &poll.name,
                                    bus.alloc_seq(),
                                    Some(data),
                                    None,
                                ),
                            );
                        }
                    }
                    ScreenOutcome::Skipped => {
                        // A failed capture backs off: a wedged worker must
                        // not hot-poll and stall the publisher tick.
                        topic.backoff_until = now + CAPTURE_BACKOFF;
                    }
                }
            }
            for (id, sub) in subs_guard.subscriptions.iter_mut() {
                for topic in sub.topics.iter_mut() {
                    if !topic.primed {
                        continue;
                    }
                    if now.duration_since(topic.framed) >= HEARTBEAT_IDLE {
                        topic.framed = now;
                        bus.push(
                            id,
                            topic.topic.name(),
                            Kind::Heartbeat,
                            event_frame(
                                proto,
                                "heartbeat",
                                id,
                                topic.topic.name(),
                                bus.alloc_seq(),
                                None,
                                Some(&info.generation),
                            ),
                        );
                    }
                }
            }
        }
    }
}

/// Drain the bounded event queue to stdout. Gap marking happens at push
/// time (per-topic latest wins), so this only moves frames in order. A
/// client that stops draining blocks here on a full pipe while the
/// publisher keeps only the latest bounded frames; a broken pipe ends
/// the thread.
pub(super) fn writer_loop(out: Arc<Mutex<std::io::Stdout>>, bus: Arc<Bus>, stop: Arc<AtomicBool>) {
    loop {
        let frame = {
            let mut guard = bus.inner.lock().unwrap();
            while guard.queue.is_empty() {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                guard = bus
                    .wake
                    .wait_timeout(guard, Duration::from_millis(250))
                    .unwrap()
                    .0;
            }
            guard.queue.pop_front().unwrap().frame
        };
        if write_packet(&mut *out.lock().unwrap(), &frame).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(bus: &Bus, sub: &str, topic: &str, kind: Kind, tag: &str) {
        let (event, data) = match kind {
            Kind::Snapshot => ("snapshot", Some(json!([{"tag": tag}]))),
            Kind::Screen => ("screen", Some(json!({"text": tag}))),
            Kind::Heartbeat => ("heartbeat", None),
            Kind::Ended => ("ended", None),
        };
        bus.push(
            sub,
            topic,
            kind,
            event_frame(2, event, sub, topic, bus.alloc_seq(), data, None),
        );
    }

    // Use exactly the writer's dequeue operation. Holding back these pops
    // establishes backpressure without depending on pipe size, polling
    // cadence, or how much CPU the helper gets during a timed deaf window.
    fn pop(bus: &Bus) -> Value {
        bus.inner.lock().unwrap().queue.pop_front().unwrap().frame
    }

    #[test]
    fn dropped_frames_resume_with_gap_and_latest_state() {
        let bus = Bus::new();
        push(&bus, "sub-1", "sessions", Kind::Snapshot, "old");
        push(&bus, "sub-1", "screen:live", Kind::Screen, "screen-v1");
        // No writer pops until all these replacements have happened.
        for tag in ["intermediate", "final-tag"] {
            push(&bus, "sub-1", "sessions", Kind::Snapshot, tag);
        }
        push(&bus, "sub-1", "screen:live", Kind::Screen, "screen-v2");
        push(&bus, "sub-1", "screen:live", Kind::Ended, "");
        // A heartbeat must not overwrite the latest state or its gap.
        push(&bus, "sub-1", "sessions", Kind::Heartbeat, "");

        let snapshot = pop(&bus);
        assert_eq!(snapshot["event"], "snapshot");
        assert_eq!(snapshot["subscription"], "sub-1");
        assert_eq!(snapshot["topic"], "sessions");
        assert_eq!(snapshot["data"], json!([{"tag": "final-tag"}]));
        assert_eq!(snapshot["gap"], true);
        let ended = pop(&bus);
        assert_eq!(ended["event"], "ended");
        assert_eq!(ended["subscription"], "sub-1");
        assert_eq!(ended["topic"], "screen:live");
        assert_eq!(ended["gap"], true);
        assert!(ended.get("data").is_none());
        assert!(bus.inner.lock().unwrap().queue.is_empty());

        push(&bus, "sub-1", "sessions", Kind::Snapshot, "after-resume");
        assert!(pop(&bus).get("gap").is_none());
    }

    #[test]
    fn final_state_without_queue_loss_has_no_gap() {
        let bus = Bus::new();
        // Legal ordering in the old integration fixture: the writer fits
        // every emitted snapshot in the pipe before the client resumes.
        // Another legal ordering publishes only final-tag after the helper
        // is descheduled across all the intermediate disk writes.
        for tag in ["hammer-0", "hammer-1", "final-tag"] {
            push(&bus, "sub-1", "sessions", Kind::Snapshot, tag);
            let frame = pop(&bus);
            assert_eq!(frame["data"], json!([{"tag": tag}]));
            assert!(frame.get("gap").is_none());
        }
        // A real gap may also have been delivered before final-tag. It is
        // per queued frame, not sticky for the rest of the deaf interval.
        push(&bus, "sub-1", "sessions", Kind::Snapshot, "old");
        push(&bus, "sub-1", "sessions", Kind::Snapshot, "new");
        assert_eq!(pop(&bus)["gap"], true);
        push(&bus, "sub-1", "sessions", Kind::Snapshot, "final-tag");
        assert!(pop(&bus).get("gap").is_none());
    }

    #[test]
    fn overflow_gap_follows_the_evicted_subscription_and_topic() {
        let bus = Bus::new();
        push(&bus, "lost", "sessions", Kind::Snapshot, "old");
        for n in 0..EVENT_QUEUE_CAP {
            push(
                &bus,
                &format!("other-{n}"),
                "sessions",
                Kind::Snapshot,
                "other",
            );
        }
        assert_eq!(bus.inner.lock().unwrap().queue.len(), EVENT_QUEUE_CAP);
        for _ in 0..EVENT_QUEUE_CAP {
            assert!(pop(&bus).get("gap").is_none());
        }
        push(&bus, "lost", "screen:live", Kind::Screen, "unaffected");
        assert!(pop(&bus).get("gap").is_none());
        push(&bus, "lost", "sessions", Kind::Snapshot, "final-tag");
        let snapshot = pop(&bus);
        assert_eq!(snapshot["data"], json!([{"tag": "final-tag"}]));
        assert_eq!(snapshot["gap"], true);
        push(&bus, "lost", "sessions", Kind::Snapshot, "after-resume");
        assert!(pop(&bus).get("gap").is_none());
    }

    #[test]
    fn ended_survives_overflow_and_inherits_an_evicted_screen_gap() {
        let bus = Bus::new();
        push(&bus, "sub-1", "screen:live", Kind::Screen, "old");
        for n in 0..EVENT_QUEUE_CAP {
            push(
                &bus,
                &format!("other-{n}"),
                "sessions",
                Kind::Snapshot,
                "other",
            );
        }
        push(&bus, "sub-1", "screen:live", Kind::Ended, "");
        for n in 0..EVENT_QUEUE_CAP {
            push(&bus, &format!("new-{n}"), "sessions", Kind::Snapshot, "new");
        }
        let guard = bus.inner.lock().unwrap();
        let ended: Vec<_> = guard
            .queue
            .iter()
            .filter(|q| q.kind == Kind::Ended)
            .collect();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].frame["topic"], "screen:live");
        assert_eq!(ended[0].frame["gap"], true);
        assert!(ended[0].frame.get("data").is_none());
    }
}
