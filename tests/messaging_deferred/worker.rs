//! Fake worker transport for deferred-delivery tests.
//!
//! Unix: a real `UnixListener` worker that records framed PTY writes.
//! Windows: an inert stub (no worker is listening, nothing is ever written);
//! only tests that expect delivery to be *rejected before transport* run there.

#[cfg(windows)]
pub(super) struct Worker;

#[cfg(windows)]
impl Worker {
    pub(super) fn start(_record: &aplexer::SessionRecord, _lose_response: bool) -> Self {
        Self
    }

    pub(super) fn finish(self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

#[cfg(unix)]
pub(super) use unix_worker::Worker;

#[cfg(unix)]
mod unix_worker {
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

    pub(crate) struct Worker {
        stop: Arc<AtomicBool>,
        thread: JoinHandle<Vec<Vec<u8>>>,
    }

    impl Worker {
        pub(crate) fn start(record: &SessionRecord, lose_response: bool) -> Self {
            std::fs::create_dir_all(record.socket_path.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(&record.socket_path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let (signal, record) = (stop.clone(), record.clone());
            let thread = thread::spawn(move || serve(listener, signal, record, lose_response));
            Self { stop, thread }
        }

        pub(crate) fn finish(self) -> Vec<Vec<u8>> {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.join().unwrap()
        }
    }

    fn serve(
        listener: UnixListener,
        stop: Arc<AtomicBool>,
        record: SessionRecord,
        lose_response: bool,
    ) -> Vec<Vec<u8>> {
        let mut writes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    respond(&mut stream, &record, &mut writes, lose_response);
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
        lose_response: bool,
    ) {
        let request: Request = frame_json(read_frame(stream).unwrap().unwrap()).unwrap();
        assert_eq!(request.session_id, Some(record.id));
        match request.operation {
            Operation::Status => write_json(
                stream,
                &Response::ok(request.request_id, serde_json::to_value(record).unwrap()),
            )
            .unwrap(),
            Operation::CaptureScreen { .. } => {
                write_json(
                    stream,
                    &Response::ok(request.request_id, json!({"bytes": 8})),
                )
                .unwrap();
                write_frame(stream, FrameKind::Data, b"\x1b[?2004h").unwrap();
            }
            Operation::Send { bytes } => {
                record_input(stream, request.request_id, bytes, writes, lose_response)
            }
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
}
