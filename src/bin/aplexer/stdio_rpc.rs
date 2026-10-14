//! PSC-RPC-1: bounded framed stdio, resident registry and atomic worker calls.
use super::capture_commands::capture_json_value;
use super::status_commands::StatusData;
use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Condvar, RwLock};

#[path = "stdio_rpc_cache.rs"]
mod cache;
#[path = "stdio_rpc_operations.rs"]
mod operations;
#[path = "stdio_rpc_psc.rs"]
mod psc;
#[path = "stdio_rpc_versions.rs"]
mod versions;
#[path = "stdio_rpc_wire.rs"]
mod wire;
use cache::{refresh_loop, usage_loop, Snapshot};
use operations::{execute, validate};
use versions::installed_versions;
use wire::{failure, read_packet, write_cached_result, write_packet};

const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const PAYLOAD_LIMIT: usize = 1024 * 1024;
const IN_FLIGHT: usize = 32;
const ID_LIMIT: usize = 65536;

struct HelperInfo {
    generation: String,
    versions: Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    protocol: u32,
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Default)]
struct SendOrder {
    issued: u64,
    serving: u64,
}
type SendGate = Arc<(Mutex<SendOrder>, Condvar)>;
struct SendTurn {
    gate: SendGate,
    ticket: u64,
}
impl SendTurn {
    fn wait(&self) {
        let (lock, wake) = &*self.gate;
        let mut order = lock.lock().unwrap();
        while order.serving != self.ticket {
            order = wake.wait(order).unwrap();
        }
    }
}
impl Drop for SendTurn {
    fn drop(&mut self) {
        let (lock, wake) = &*self.gate;
        lock.lock().unwrap().serving += 1;
        wake.notify_all();
    }
}

pub(crate) fn run(paths: Paths, protocol: u32) -> Result<()> {
    if protocol != 1 {
        bail!("unsupported stdio RPC protocol {protocol}");
    }
    let versions = installed_versions();
    let state = Arc::new(RwLock::new(Snapshot::load(&paths)?));
    let stop = Arc::new(AtomicBool::new(false));
    let refresher = {
        let (p, s, t) = (paths.clone(), state.clone(), stop.clone());
        thread::spawn(move || refresh_loop(p, s, t))
    };
    let sampler = {
        let (p, s, t) = (paths.clone(), state.clone(), stop.clone());
        thread::spawn(move || usage_loop(p, s, t))
    };
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let admission = Arc::new((Mutex::new(0usize), Condvar::new()));
    let info = Arc::new(HelperInfo {
        generation: Uuid::new_v4().to_string(),
        versions,
    });
    let mut ids = HashSet::new();
    let mut gates: HashMap<Uuid, SendGate> = HashMap::new();
    // Reuse bounded worker threads: no OS thread spawn on a warm request.
    type Job = Box<dyn FnOnce() + Send>;
    let (jobs, receiver) = std::sync::mpsc::sync_channel::<Job>(IN_FLIGHT);
    let receiver = Arc::new(Mutex::new(receiver));
    let workers: Vec<_> = (0..IN_FLIGHT)
        .map(|_| {
            let receiver = receiver.clone();
            thread::spawn(move || loop {
                let job = receiver.lock().unwrap().recv();
                match job {
                    Ok(job) => job(),
                    Err(_) => break,
                }
            })
        })
        .collect();
    let result = (|| -> Result<()> {
        let mut stdin = io::stdin().lock();
        while let Some(bytes) = read_packet(&mut stdin)? {
            let raw: Value = serde_json::from_slice(&bytes).context("invalid RPC JSON")?;
            let id = raw.get("id").cloned().unwrap_or(Value::Null);
            let call: Call = match serde_json::from_value(raw) {
                Ok(c) => c,
                Err(e) => {
                    write_packet(
                        &mut *stdout.lock().unwrap(),
                        &failure(id, "invalid_request", e, true),
                    )?;
                    continue;
                }
            };
            let reject = if call.protocol != 1 {
                Some(("unsupported_protocol", "unsupported protocol".to_owned()))
            } else if ids.contains(&call.id) {
                Some(("duplicate_id", "request id already used".to_owned()))
            } else if ids.len() >= ID_LIMIT {
                Some((
                    "invalid_request",
                    "request id capacity exhausted; reconnect".to_owned(),
                ))
            } else {
                validate(&call).err().map(|e| {
                    (
                        if call.method != "capabilities"
                            && !["ping", "list", "status", "capture", "send"]
                                .contains(&call.method.as_str())
                        {
                            "unsupported_method"
                        } else {
                            "invalid_request"
                        },
                        e.to_string(),
                    )
                })
            };
            if let Some((code, message)) = reject {
                write_packet(
                    &mut *stdout.lock().unwrap(),
                    &failure(id, code, message, true),
                )?;
                continue;
            }
            ids.insert(call.id.clone());
            if call.method == "list" && !call.params["running"].as_bool().unwrap_or(false) {
                let (data, meta, error) = {
                    let guard = state.read().unwrap();
                    (
                        if call.params["format"] == "psc" {
                            guard.psc_bytes.clone()
                        } else {
                            guard.list_bytes.clone()
                        },
                        guard.meta(),
                        guard.unavailable(),
                    )
                };
                if let Some(e) = error {
                    write_packet(
                        &mut *stdout.lock().unwrap(),
                        &failure(id, "registry_unavailable", e, true),
                    )?;
                } else {
                    write_cached_result(&mut *stdout.lock().unwrap(), &id, &data, &meta)?;
                }
                continue;
            }
            let (record, row, meta) = {
                let guard = state.read().unwrap();
                if let Some(e) = guard.unavailable() {
                    write_packet(
                        &mut *stdout.lock().unwrap(),
                        &failure(id, "registry_unavailable", e, true),
                    )?;
                    continue;
                }
                let record = if let Some(selector) = call.params["selector"].as_str() {
                    let resolved = if call.params["selectorMode"] == "psc" {
                        match psc::resolve(&guard.records, &guard.rows, selector) {
                            Ok(r) => Ok(r),
                            Err(error) => {
                                write_packet(
                                    &mut *stdout.lock().unwrap(),
                                    &json!({"protocol":1,"id":id,"error":error}),
                                )?;
                                continue;
                            }
                        }
                    } else {
                        resolve_records(&guard.records, Some(selector), None, None)
                    };
                    match resolved {
                        Ok(r) => Some(r),
                        Err(e) => {
                            let code = if e.to_string() == NO_MATCHING_SESSION {
                                "not_found"
                            } else {
                                "ambiguous"
                            };
                            write_packet(
                                &mut *stdout.lock().unwrap(),
                                &failure(id, code, e, true),
                            )?;
                            continue;
                        }
                    }
                } else {
                    None
                };
                let row = record.as_ref().and_then(|r| {
                    guard
                        .rows
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|v| v["id"].as_str() == Some(&r.id.to_string()))
                        .cloned()
                });
                (record, row, guard.meta())
            };
            let admitted = Instant::now();
            let turn = if call.method == "send" {
                let gate = gates
                    .entry(record.as_ref().unwrap().id)
                    .or_default()
                    .clone();
                let ticket = {
                    let mut order = gate.0.lock().unwrap();
                    let n = order.issued;
                    order.issued += 1;
                    n
                };
                Some(SendTurn { gate, ticket })
            } else {
                None
            };
            let (lock, wake) = &*admission;
            let mut active = lock.lock().unwrap();
            while *active >= IN_FLIGHT {
                active = wake.wait(active).unwrap();
            }
            *active += 1;
            drop(active);
            let (p, s, out, admit, info) = (
                paths.clone(),
                state.clone(),
                stdout.clone(),
                admission.clone(),
                info.clone(),
            );
            jobs.send(Box::new(move || {
                if let Some(t) = &turn {
                    t.wait();
                }
                let mut dispatched = false;
                let result = execute(&p, &call, record, row, &mut dispatched, &info, &s);
                let mut meta = meta;
                for field in ["ageMs", "processAgeMs"] {
                    meta[field] = json!(meta[field]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_add(admitted.elapsed().as_millis() as u64));
                }
                let response = match result {
                    Ok(result) => json!({"protocol":1,"id":call.id,"result":result,"meta":meta}),
                    Err(e) => failure(
                        json!(call.id),
                        "operation_failed",
                        format!("{e:#}"),
                        !dispatched,
                    ),
                };
                let _ = write_packet(&mut *out.lock().unwrap(), &response);
                drop(turn);
                let (lock, wake) = &*admit;
                *lock.lock().unwrap() -= 1;
                wake.notify_all();
            }))
            .map_err(|_| anyhow!("RPC worker pool disconnected"))?;
        }
        Ok(())
    })();
    drop(jobs);
    let (lock, wake) = &*admission;
    let mut active = lock.lock().unwrap();
    while *active != 0 {
        active = wake.wait(active).unwrap();
    }
    for worker in workers {
        let _ = worker.join();
    }
    stop.store(true, Ordering::Relaxed);
    let _ = refresher.join();
    let _ = sampler.join();
    result
}
