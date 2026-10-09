//! Fake worker transport for deferred-delivery tests: a real control-socket
//! listener (Unix domain socket, or a named pipe on Windows) that records
//! framed PTY writes, and paints an agent composer for `CaptureScreen`: an
//! empty `❯` prompt, the pending draft until a carriage return submits it,
//! or -- `start_without_prompt` -- no prompt at all (a dialog, or busy).

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
            Self::spawn(record, lose_response, Mode::Prompt)
        }

        pub(crate) fn start_without_prompt(record: &SessionRecord) -> Self {
            Self::spawn(record, false, Mode::NoPrompt)
        }

        /// The reported bug: Enter lands inside the draft as a newline.
        pub(crate) fn start_swallowing_enter(record: &SessionRecord) -> Self {
            Self::spawn(record, false, Mode::SwallowEnter)
        }

        fn spawn(record: &SessionRecord, lose_response: bool, mode: Mode) -> Self {
            // The Unix socket lives under the runtime dir; a Windows pipe name has no
            // parent directory.
            #[cfg(unix)]
            std::fs::create_dir_all(record.socket_path.parent().unwrap()).unwrap();
            let listener = Listener::bind(&record.socket_path).unwrap();
            #[cfg(unix)]
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let (signal, record) = (stop.clone(), record.clone());
            let thread =
                thread::spawn(move || serve(listener, signal, record, lose_response, mode));
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
        mode: Mode,
    ) -> Vec<Vec<u8>> {
        let mut writes = Vec::new();
        let mut screen = Screen {
            mode,
            draft: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match accept(&listener) {
                Ok(Some(mut stream)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    respond(
                        &mut stream,
                        &record,
                        &mut writes,
                        &mut screen,
                        lose_response,
                    );
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

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Prompt,
        NoPrompt,
        SwallowEnter,
    }

    struct Screen {
        mode: Mode,
        draft: Vec<u8>,
    }

    impl Screen {
        fn input(&mut self, bytes: &[u8]) {
            match bytes {
                b"\r" if self.mode == Mode::SwallowEnter => self.draft.push(b'\n'),
                b"\r" => self.draft.clear(),
                b"\x1b[200~" | b"\x1b[201~" => {}
                text => self.draft.extend_from_slice(text),
            }
        }

        fn paint(&self) -> Vec<u8> {
            let mut out = b"\x1b[?2004h".to_vec();
            if self.mode == Mode::NoPrompt {
                return out;
            }
            // One row per draft line, cursor after the last: an Enter read
            // as a newline leaves the head row and moves the cursor down.
            let draft = String::from_utf8_lossy(&self.draft);
            let mut lines: Vec<String> = draft
                .split('\n')
                .map(|l| l.chars().take(60).collect())
                .collect();
            if lines.len() > 20 {
                lines.drain(1..lines.len() - 19);
            }
            for (row, line) in lines.iter().enumerate() {
                let glyph = if row == 0 { "❯" } else { " " };
                out.extend_from_slice(format!("\x1b[{};1H{glyph} {line}", row + 2).as_bytes());
            }
            let last = lines.last().map_or(0, |l| l.chars().count());
            out.extend_from_slice(format!("\x1b[{};{}H", lines.len() + 1, 3 + last).as_bytes());
            out
        }
    }

    fn respond(
        stream: &mut Stream,
        record: &SessionRecord,
        writes: &mut Vec<Vec<u8>>,
        screen: &mut Screen,
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
                let painted = screen.paint();
                write_json(
                    stream,
                    &Response::ok(request.request_id, json!({"bytes": painted.len()})),
                )
                .unwrap();
                write_frame(stream, FrameKind::Data, &painted).unwrap();
            }
            Operation::Send { bytes } => {
                record_input(stream, request.request_id, bytes, writes, lose_response);
                screen.input(writes.last().unwrap());
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
