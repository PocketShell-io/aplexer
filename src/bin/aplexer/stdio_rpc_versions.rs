//! One bounded cold version probe; no subprocess on warm requests.
use super::*;

pub(super) fn installed_versions() -> Value {
    let mut pocketshell = Value::Null;
    if let Ok(mut child) = Command::new("pocketshell")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let output = child.stdout.take().unwrap();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = output.take(4096).read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        if let Ok(bytes) =
                            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        {
                            if let Ok(text) = String::from_utf8(bytes) {
                                if let Some(line) = text.lines().last() {
                                    pocketshell = json!(line.trim());
                                }
                            }
                        }
                    }
                    break;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
    json!({"aplexer":format!("aplexer {}",env!("CARGO_PKG_VERSION")),"pocketshell":pocketshell})
}
