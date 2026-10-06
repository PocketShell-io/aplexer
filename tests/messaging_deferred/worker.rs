//! Fake worker transport for deferred-delivery tests: a real control-socket
//! listener (Unix domain socket, or a named pipe on Windows) that records
//! framed PTY writes.

pub(super) use fake_worker::Worker;

mod fake_worker {
    use aplexer::sys::ipc::{Listener, Stream};
    use aplexer::{
        frame_json, read_frame, write_frame, write_json, FrameKind, Operation, Request, Response,
        SessionRecord,
    };
    use serde_json::json;
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
            // The Unix socket lives under the runtime dir; a Windows pipe name has no
            // parent directory.
            #[cfg(unix)]
            std::fs::create_dir_all(record.socket_path.parent().unwrap()).unwrap();
            let listener = Listener::bind(&record.socket_path).unwrap();
            #[cfg(unix)]
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
        listener: Listener,
        stop: Arc<AtomicBool>,
        record: SessionRecord,
        lose_response: bool,
    ) -> Vec<Vec<u8>> {
        let mut writes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match accept(&listener) {
                Ok(Some(mut stream)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    respond(&mut stream, &record, &mut writes, lose_response);
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("{error}"),
            }
        }
        writes
    }

    /// One non-blocking accept: `Ok(None)` when nobody is connecting yet.
    #[cfg(unix)]
    fn accept(listener: &Listener) -> std::io::Result<Option<Stream>> {
        match listener.accept() {
            Ok((stream, _)) => Ok(Some(stream)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    #[cfg(windows)]
    fn accept(listener: &Listener) -> std::io::Result<Option<Stream>> {
        listener.accept_timeout(Duration::from_millis(50))
    }

    fn respond(
        stream: &mut Stream,
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
        stream: &mut Stream,
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
