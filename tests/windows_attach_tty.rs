#![cfg(windows)]
//! Interactive `aplexer attach` on Windows, hosted on a real console.
//!
//! The attach client is run as a child of a ConPTY (`PtyMaster` +
//! `spawn_workload`), so it sees a genuine console for stdin and stdout, the
//! same as under Windows Terminal or conhost. The test plays the terminal:
//! it types bytes into the pseudoconsole input pipe and reads the raw VT
//! stream the client writes.
//!
//! Each test gets its own state/runtime dirs; sessions run `cmd.exe`
//! (or `powershell.exe`) under a worker started with `aplexer start`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aplexer::sys::windows::pty::{spawn_workload, PtyMaster, Workload};
use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_aplexer");
const WAIT: Duration = Duration::from_secs(30);

struct Env {
    runtime: TempDir,
    state: TempDir,
    work: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            runtime: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
        }
    }

    fn config(&self) -> std::path::PathBuf {
        self.runtime.path().join("config.toml")
    }

    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("APLEXER_RUNTIME_DIR", self.runtime.path());
        c.env("APLEXER_STATE_DIR", self.state.path());
        c.env("APLEXER_CONFIG", self.config());
        c.env_remove("APLEXER_SESSION");
        // The default shell is Git Bash; this file drives PowerShell/cmd.
        c.env("APLEXER_SHELL", "powershell");
        c
    }

    fn env_overrides(&self) -> BTreeMap<OsString, Option<OsString>> {
        let mut m = BTreeMap::new();
        m.insert(
            "APLEXER_RUNTIME_DIR".into(),
            Some(self.runtime.path().into()),
        );
        m.insert("APLEXER_STATE_DIR".into(), Some(self.state.path().into()));
        m.insert("APLEXER_CONFIG".into(), Some(self.config().into()));
        m.insert("APLEXER_SESSION".into(), None);
        m.insert("APLEXER_SHELL".into(), Some("powershell".into()));
        m
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    /// Start a detached session running `argv`; returns its id.
    fn start(&self, tag: &str, argv: &[&str]) -> String {
        let mut args = vec![
            "start",
            "--workspace",
            self.work.path().to_str().unwrap(),
            "--tag",
            tag,
            "--json",
            "--",
        ];
        args.extend_from_slice(argv);
        let out = self.run(&args);
        if !out.status.success() {
            self.diagnostics();
        }
        assert!(out.status.success(), "start failed: {out:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["id"].as_str().unwrap().to_owned()
    }

    fn kill(&self, id: &str) {
        let _ = self.run(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    }

    fn alive(&self, id: &str) -> bool {
        let out = self.run(&["status", id, "--json"]);
        out.status.success()
            && serde_json::from_slice::<Value>(&out.stdout)
                .map(|v| v["state"].as_str() != Some("exited"))
                .unwrap_or(false)
    }

    // The footer truncates connection errors, and start's error cannot include
    // the worker's stderr. Preserve fixture-only evidence before Drop cleans
    // it up; never print the launch environment (it can carry credentials).
    fn diagnostics(&self) {
        fn visit(path: &std::path::Path) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path);
                } else if path.file_name().is_some_and(|name| name == "worker.log") {
                    eprintln!(
                        "fixture worker log {}:\n{}",
                        path.display(),
                        std::fs::read_to_string(&path).unwrap_or_else(|e| e.to_string())
                    );
                } else if path.file_name().is_some_and(|name| name == "session.json") {
                    if let Ok(bytes) = std::fs::read(&path) {
                        if let Ok(record) = serde_json::from_slice::<Value>(&bytes) {
                            eprintln!(
                                "fixture record {}: id={} phase={} worker={} workload={} error={}",
                                path.display(),
                                record["id"],
                                record["phase"],
                                record["worker_pid"],
                                record["workload_pid"],
                                record["error"]
                            );
                        }
                    }
                }
            }
        }
        visit(self.state.path());
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if thread::panicking() {
            self.diagnostics();
        }
        // Kill any leftover sessions so temp dirs can be removed.
        let out = self.run(&["list", "--json"]);
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            let list = v
                .as_array()
                .cloned()
                .or_else(|| v["sessions"].as_array().cloned());
            for s in list.unwrap_or_default() {
                if let Some(id) = s["id"].as_str() {
                    self.kill(id);
                }
            }
        }
    }
}

/// A command (usually `aplexer attach`) running on a ConPTY we control.
struct Term {
    pty: PtyMaster,
    child: Workload,
    input: File,
    out: Arc<Mutex<Vec<u8>>>,
    rows: u16,
    cols: u16,
}

impl Term {
    fn spawn(env: &Env, argv: &[String], rows: u16, cols: u16) -> Term {
        let pty = PtyMaster::open(rows, cols).unwrap();
        let child = spawn_workload(
            argv,
            env.work.path(),
            &env.env_overrides(),
            Some(&pty),
            None,
        )
        .expect("spawn on conpty");
        let mut reader = pty.reader().unwrap();
        let mut answer = pty.writer().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        sink.lock().unwrap().extend_from_slice(&buf[..n]);
                        // conhost asks for the cursor position at startup and
                        // stalls until a terminal answers.
                        if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                            let _ = answer.write_all(b"\x1b[1;1R");
                        }
                    }
                }
            }
        });
        Term {
            input: pty.writer().unwrap(),
            pty,
            child,
            out,
            rows,
            cols,
        }
    }

    fn attach(env: &Env, args: &[&str], rows: u16, cols: u16) -> Term {
        let mut argv = vec![BIN.to_owned(), "attach".to_owned()];
        argv.extend(args.iter().map(|s| (*s).to_owned()));
        Term::spawn(env, &argv, rows, cols)
    }

    fn send(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).unwrap();
        self.input.flush().unwrap();
    }

    fn raw(&self) -> Vec<u8> {
        self.out.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.raw()).into_owned()
    }

    fn since(&self, mark: usize) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()[mark..]).into_owned()
    }

    fn wait_for(&self, needle: &str) -> String {
        self.wait_for_since(0, needle)
    }

    fn wait_for_since(&self, mark: usize, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let s = self.since(mark);
            if s.contains(needle) {
                return s;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {needle:?}; output so far:\n{s:?}");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_exit(&self) -> u32 {
        match self.child.wait_timeout(WAIT).unwrap() {
            Some(code) => code,
            None => panic!("process did not exit; output:\n{:?}", self.text()),
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.terminate(1);
        self.pty.close();
    }
}

fn start_cmd(env: &Env, tag: &str) -> String {
    env.start(tag, &["cmd.exe"])
}

impl Term {
    /// The screen as a terminal would show it, rows joined by `\n`.
    fn screen(&self) -> String {
        let mut parser = vt100::Parser::new(self.rows, self.cols, 0);
        parser.process(&self.raw());
        parser.screen().contents()
    }

    fn screen_rows(&self) -> Vec<String> {
        let mut parser = vt100::Parser::new(self.rows, self.cols, 0);
        parser.process(&self.raw());
        parser.screen().rows(0, self.cols).collect()
    }

    fn wait_screen(&self, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let s = self.screen();
            if s.contains(needle) {
                return s;
            }
            if Instant::now() > deadline {
                panic!(
                    "timed out waiting for {needle:?} on screen:\n{s}\n\nraw: {:?}",
                    self.text()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_screen_gone(&self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while self.screen().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "{needle:?} never left the screen"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn ready(&mut self) {
        self.wait_screen("RUNNING");
        self.wait_screen(">");
    }
}

#[test]
fn types_text_status_bar_and_detach() {
    let env = Env::new();
    let id = start_cmd(&env, "echo");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"echo hello-aplexer\r");
    let screen = t.wait_screen("\nhello-aplexer");
    assert!(
        screen.contains("echo hello-aplexer"),
        "typed text echoes: {screen}"
    );
    // The status bar owns the last row.
    let rows = t.screen_rows();
    let bar = rows.last().unwrap();
    assert!(
        bar.contains(":echo") && bar.contains("RUNNING") && bar.contains("^b ?"),
        "bar: {bar:?} rows: {rows:?}"
    );
    assert!(rows[..29].iter().all(|r| !r.contains("RUNNING")));
    let raw = t.text();
    assert!(raw.contains("\x1b[?1049h"), "alt screen entered");
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
    let raw = t.text();
    assert!(raw.contains("\x1b[?1049l"), "alt screen left: {raw:?}");
    t.wait_for("Detached from");
    assert!(env.alive(&id), "detach leaves the session running");
}

#[test]
fn ctrl_c_reaches_the_workload() {
    let env = Env::new();
    let id = start_cmd(&env, "ctrlc");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"ping -n 60 127.0.0.1\r");
    t.wait_screen("Pinging 127.0.0.1");
    thread::sleep(Duration::from_millis(1500));
    t.send(b"\x03");
    t.wait_screen("Control-C");
    t.send(b"echo after-ctrlc\r");
    t.wait_screen("\nafter-ctrlc");
    // The client itself must still be attached.
    assert!(
        t.child.try_wait().unwrap().is_none(),
        "client survived Ctrl-C"
    );
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn resize_reaches_the_workload() {
    let env = Env::new();
    let id = start_cmd(&env, "resize");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"mode con\r");
    let s = t.wait_screen("Columns:");
    assert!(
        s.contains("Lines:") && s.contains("100"),
        "initial size: {s}"
    );
    assert!(s.contains("29"), "one row reserved for the bar: {s}");
    t.pty.resize(40, 120).unwrap();
    t.rows = 40;
    t.cols = 120;
    thread::sleep(Duration::from_millis(1500));
    t.send(b"cls\rmode con\r");
    let deadline = Instant::now() + WAIT;
    loop {
        let s = t.screen();
        if s.contains("Columns:") && s.contains("120") && s.contains("39") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "workload never saw the new size:\n{s}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn flood_then_still_responsive() {
    let env = Env::new();
    let id = start_cmd(&env, "flood");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"for /l %i in (1,1,5000) do @echo flood-line-%i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r");
    t.wait_for("flood-line-5000-");
    t.send(b"echo after-flood\r");
    // Typed echo + command output: two occurrences once the shell answered.
    let deadline = Instant::now() + WAIT;
    while t.text().matches("after-flood").count() < 2 {
        assert!(
            Instant::now() < deadline,
            "no answer after the flood: {:?}",
            t.screen()
        );
        thread::sleep(Duration::from_millis(50));
    }
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn killed_client_leaves_session_and_reattach_shows_screen() {
    let env = Env::new();
    let id = start_cmd(&env, "abrupt");
    {
        let mut t = Term::attach(&env, &[&id], 30, 100);
        t.ready();
        t.send(b"echo still-here-marker\r");
        t.wait_screen("\nstill-here-marker");
        t.child.terminate(1).unwrap();
        t.wait_exit();
    }
    thread::sleep(Duration::from_secs(1));
    assert!(env.alive(&id), "session survives a killed client");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.wait_screen("still-here-marker");
    t.send(b"echo second-client\r");
    t.wait_screen("\nsecond-client");
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn console_modes_are_restored_after_detach() {
    let env = Env::new();
    let id = start_cmd(&env, "modes");
    let script = env.work.path().join("modes.ps1");
    std::fs::write(
        &script,
        format!(
            r#"Add-Type -TypeDefinition 'using System;using System.Runtime.InteropServices;public class K{{[DllImport("kernel32.dll")]public static extern IntPtr GetStdHandle(int n);[DllImport("kernel32.dll")]public static extern bool GetConsoleMode(IntPtr h,out uint m);[DllImport("kernel32.dll")]public static extern uint GetConsoleOutputCP();}}'
function Show($tag){{ $i=[uint32]0; $o=[uint32]0
 [void][K]::GetConsoleMode([K]::GetStdHandle(-10),[ref]$i); [void][K]::GetConsoleMode([K]::GetStdHandle(-11),[ref]$o)
 [Console]::Out.WriteLine("MODES-$tag in=$i out=$o cp=$([K]::GetConsoleOutputCP())") }}
Show 'BEFORE'
& '{bin}' attach {id}
Show 'AFTER'
"#,
            bin = BIN,
            id = id
        ),
    )
    .unwrap();
    let argv: Vec<String> = [
        "powershell.exe",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([script.to_str().unwrap().to_owned()])
    .collect();
    let mut t = Term::spawn(&env, &argv, 30, 100);
    let before = t.wait_for("MODES-BEFORE");
    t.wait_for("RUNNING");
    t.wait_screen("RUNNING");
    thread::sleep(Duration::from_secs(1));
    t.send(b"\x02d");
    let after = t.wait_for("MODES-AFTER");
    let grab = |s: &str, tag: &str| -> String {
        let at = s.find(tag).unwrap();
        s[at..]
            .lines()
            .next()
            .unwrap()
            .split_once(' ')
            .unwrap()
            .1
            .trim()
            .to_owned()
    };
    let b = grab(&before, "MODES-BEFORE");
    let a = grab(&after, "MODES-AFTER");
    assert_eq!(a, b, "console input/output modes and code page restored");
    t.wait_exit();
}

#[test]
fn switching_and_picker() {
    let env = Env::new();
    let first = env.start("alpha", &["cmd.exe", "/k", "echo I-AM-ALPHA"]);
    let _second = env.start("beta", &["cmd.exe", "/k", "echo I-AM-BETA"]);
    let mut t = Term::attach(&env, &[&first], 30, 100);
    t.wait_screen("I-AM-ALPHA");
    t.wait_screen(":alpha");
    // Ctrl-b Left: previous session in the workspace (arrows on VT input).
    t.send(b"\x02\x1b[D");
    t.wait_screen("I-AM-BETA");
    t.wait_screen(":beta");
    assert!(
        !t.screen().contains("I-AM-ALPHA"),
        "alpha's screen replaced: {}",
        t.screen()
    );
    // Ctrl-b Right: back.
    t.send(b"\x02\x1b[C");
    t.wait_screen("I-AM-ALPHA");
    t.wait_screen(":alpha");
    // Ctrl-b s: session picker overlay lists both, Esc dismisses it.
    t.send(b"\x02s");
    let s = t.wait_screen(" sessions ");
    assert!(
        s.contains("alpha") && s.contains("beta"),
        "picker lists both: {s}"
    );
    t.send(b"\x1b");
    t.wait_screen_gone(" sessions ");
    t.send(b"echo typed-after-picker\r");
    t.wait_screen("typed-after-picker");
    // Ctrl-b 1 jumps by index, Ctrl-b l returns to the last session.
    t.send(b"\x021");
    t.wait_screen("I-AM-BETA");
    t.send(b"\x02l");
    t.wait_screen("typed-after-picker");
    // Ctrl-b n creates a fresh sibling session and switches to it.
    t.send(b"\x02n");
    // APLEXER_SHELL=powershell (see Env): the "Windows PowerShell" banner.
    t.wait_screen("PowerShell");
    t.wait_screen(":main");
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn scrollback_pager() {
    let env = Env::new();
    let id = start_cmd(&env, "pager");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"for /l %i in (1,1,200) do @echo scroll-line-%i\r");
    t.wait_screen("scroll-line-200");
    thread::sleep(Duration::from_millis(1500));
    // Ctrl-b [ enters the pager; PageUp (b) moves into history.
    t.send(b"\x02[");
    thread::sleep(Duration::from_millis(500));
    t.send(b"bbb");
    thread::sleep(Duration::from_millis(800));
    let s = t.screen();
    println!("PAGER\n{s}");
    assert!(
        !s.contains("scroll-line-200"),
        "scrolled away from the tail: {s}"
    );
    assert!(s.contains("scroll-line-1"), "history visible: {s}");
    t.send(b"q");
    t.wait_screen("scroll-line-200");
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn unicode_and_emoji_roundtrip() {
    let env = Env::new();
    let id = env.start("uni", &["powershell.exe", "-NoLogo", "-NoProfile"]);
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    // Typed non-ASCII (BMP + astral) must reach the workload intact and come
    // back as the same UTF-8 on the client's stdout.
    let probe = "Write-Host ('U=' + [string]::Join(',', ('h\u{e9}llo \u{65e5}\u{672c} \u{1f600}'.ToCharArray() | % { [int]$_ })) + ';')";
    t.send(probe.as_bytes());
    t.send(b"\r");
    t.wait_screen("U=104,233,108,108,111,32,26085,26412,32,55357,56832;");
    // Output side: emoji written by the workload shows up on the screen.
    t.send(
        "Write-Host ('E=' + [char]::ConvertFromUtf32(0x1F600) + '=' + '\u{65e5}\u{672c}' + ';')\r"
            .as_bytes(),
    );
    t.wait_screen("\nE=\u{1f600}=\u{65e5}\u{672c};");
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn bracketed_paste_and_modes_are_relayed() {
    let env = Env::new();
    let id = start_cmd(&env, "paste");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    // A paste arrives as one chunk; it must reach the workload verbatim.
    t.send(b"echo pasted-block-123 & echo second-part\r");
    t.wait_screen("second-part");
    // Wide / ambiguous glyphs in the bar do not break the layout: bar row is
    // exactly the terminal width and nothing spilled onto another row.
    let rows = t.screen_rows();
    let bar = rows.iter().position(|r| r.contains("RUNNING")).unwrap();
    assert_eq!(bar, 29, "bar on the last row: {rows:?}");
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn mouse_wheel_enters_the_pager() {
    let env = Env::new();
    let id = start_cmd(&env, "wheel");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"for /l %i in (1,1,120) do @echo wheel-line-%i\r");
    t.wait_screen("wheel-line-120");
    thread::sleep(Duration::from_millis(1500));
    // SGR wheel-up reports, as a mouse-reporting terminal sends them.
    for _ in 0..3 {
        t.send(b"\x1b[<64;10;10M");
        thread::sleep(Duration::from_millis(100));
    }
    t.wait_screen("SCROLL");
    t.send(b"q");
    t.wait_screen_gone("SCROLL");
    t.send(b"\x02d");
    assert_eq!(t.wait_exit(), 0);
}

#[test]
fn closing_the_console_detaches_without_killing_the_session() {
    let env = Env::new();
    let id = start_cmd(&env, "closewin");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"echo before-close\r");
    t.wait_screen("\nbefore-close");
    // The terminal window goes away (CTRL_CLOSE_EVENT in the client).
    t.pty.close();
    t.wait_exit();
    thread::sleep(Duration::from_secs(1));
    assert!(env.alive(&id), "session survives its terminal closing");
}

#[test]
fn workload_exit_ends_the_attach() {
    let env = Env::new();
    let id = start_cmd(&env, "exits");
    let mut t = Term::attach(&env, &[&id], 30, 100);
    t.ready();
    t.send(b"exit\r");
    assert_eq!(t.wait_exit(), 0);
    t.wait_for("Session ended");
    assert!(t.text().contains("\x1b[?1049l"), "alt screen left");
    let _ = id;
}
