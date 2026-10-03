use aplexer::{
    frame_json, read_frame, write_frame, write_json, FrameKind, Operation, Request, Response,
    SessionRecord,
};
use serde_json::json;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) struct Worker {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Vec<Vec<u8>>>,
}

#[derive(Default, Clone)]
pub(super) struct WorkerConfig {
    pub(super) lose_response: bool,
    pub(super) drop_screen_echo: bool,
    pub(super) draft_in_composer: Option<String>,
}

impl Worker {
    pub(super) fn start(record: &SessionRecord, lose_response: bool) -> Self {
        Self::start_with_config(
            record,
            WorkerConfig {
                lose_response,
                ..Default::default()
            },
        )
    }

    pub(super) fn start_with_config(record: &SessionRecord, config: WorkerConfig) -> Self {
        std::fs::create_dir_all(record.socket_path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&record.socket_path);
        let listener = UnixListener::bind(&record.socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (signal, record) = (stop.clone(), record.clone());
        let thread = thread::spawn(move || serve(listener, signal, record, config));
        Self { stop, thread }
    }

    pub(super) fn finish(self) -> Vec<Vec<u8>> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().unwrap()
    }
}

fn serve(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    record: SessionRecord,
    config: WorkerConfig,
) -> Vec<Vec<u8>> {
    let mut writes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                respond(&mut stream, &record, &mut writes, &config);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("{error}"),
        }
    }
    writes
}

fn respond(
    stream: &mut UnixStream,
    record: &SessionRecord,
    writes: &mut Vec<Vec<u8>>,
    config: &WorkerConfig,
) {
    let request: Request = frame_json(read_frame(stream).unwrap().unwrap()).unwrap();
    assert_eq!(request.session_id, Some(record.id));
    match request.operation {
        Operation::Status => write_json(
            stream,
            &Response::ok(request.request_id, serde_json::to_value(record).unwrap()),
        )
        .unwrap(),
        Operation::CaptureScreen { plain } => {
            let mut payload = Vec::new();
            if !plain {
                payload.extend_from_slice(b"\x1b[?2004h");
            }
            if let Some(draft) = &config.draft_in_composer {
                payload.extend_from_slice(format!("│ ❯ {draft} │\n").as_bytes());
            } else if !config.drop_screen_echo && !writes.is_empty() {
                for w in writes.iter() {
                    payload.extend_from_slice(w);
                }
            } else {
                match record.engine.as_str() {
                    "opencode" => {
                        payload.extend_from_slice(
                            "  ┃  Build auto · Muse Spark 1.3 Contributor OpenCode Go\n  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n   /home/alexey/git/test-ws  ctrl+p commands\n".as_bytes(),
                        );
                    }
                    "grok" => {
                        payload.extend_from_slice(
                            "  ╭───────────────────────────────────────╮\n  │ ❯                                     │\n  ╰────────────── Grok 4.7 · always-approve ╯\n  Shift+Tab:mode │ Ctrl+.:shortcuts\n".as_bytes(),
                        );
                    }
                    "shell" => {
                        payload.extend_from_slice(b"alexey@host:~$ ");
                    }
                    _ => {
                        payload.extend_from_slice(
                            "› Ask Codex to do anything\n  glm-5.3-flash max · test-ws · Normal interactive session requested\n".as_bytes(),
                        );
                    }
                }
            }
            write_json(
                stream,
                &Response::ok(request.request_id, json!({"bytes": payload.len()})),
            )
            .unwrap();
            write_frame(stream, FrameKind::Data, &payload).unwrap();
        }
        Operation::Capture { .. } => {
            let mut payload = Vec::new();
            if !config.drop_screen_echo {
                for w in writes.iter() {
                    payload.extend_from_slice(w);
                }
            }
            write_json(
                stream,
                &Response::ok(request.request_id, json!({"bytes": payload.len()})),
            )
            .unwrap();
            write_frame(stream, FrameKind::Data, &payload).unwrap();
        }
        Operation::Send { bytes } => record_input(
            stream,
            request.request_id,
            bytes,
            writes,
            config.lose_response,
        ),
        unexpected => panic!("unexpected RPC: {unexpected:?}"),
    }
}

fn record_input(
    stream: &mut UnixStream,
    id: String,
    bytes: usize,
    writes: &mut Vec<Vec<u8>>,
    lose_response: bool,
) {
    let frame = read_frame(stream).unwrap().unwrap();
    assert_eq!(frame.kind, FrameKind::Data);
    assert_eq!(frame.payload.len(), bytes);
    writes.push(frame.payload);
    if !lose_response {
        write_json(stream, &Response::ok(id, json!({}))).unwrap();
    }
}
