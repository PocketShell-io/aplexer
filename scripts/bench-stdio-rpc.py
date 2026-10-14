#!/usr/bin/env python3
"""Benchmark owned fixtures through a local helper; never use the live registry."""
import argparse
import base64
import hashlib
import json
import os
import pathlib
import select
import statistics
import struct
import subprocess
import tempfile
import time
import uuid


def quantiles(values):
    values = sorted(values)
    return {
        "p50": round(statistics.median(values), 3),
        "p95": round(values[int(.95 * (len(values) - 1))], 3),
        "p99": round(values[int(.99 * (len(values) - 1))], 3),
        "max": round(max(values), 3),
    }


def read_exact(stream, size):
    chunks = []
    deadline = time.monotonic() + 5
    while size:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([stream], [], [], remaining)[0]:
            raise RuntimeError("helper response timed out")
        chunk = os.read(stream.fileno(), size)
        if not chunk:
            raise RuntimeError("helper closed before completing response")
        chunks.append(chunk)
        size -= len(chunk)
    return b"".join(chunks)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/aplexer")
    parser.add_argument("--sessions", type=int, default=100)
    parser.add_argument("--runs", type=int, default=1000)
    parser.add_argument("--live", action="store_true")
    args = parser.parse_args()
    if args.sessions < 2 or args.runs < 100:
        parser.error("use >=2 sessions and >=100 samples")
    binary = str(pathlib.Path(args.binary).resolve())
    binary_hash = hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest()
    load_start = os.getloadavg()
    with tempfile.TemporaryDirectory(prefix="aplexer-rpc-bench-") as directory:
        root = pathlib.Path(directory)
        runtime, state = root / "runtime", root / "state"
        runtime.mkdir(mode=0o700)
        state.mkdir(mode=0o700)
        (state / "sessions").mkdir()
        env = {
            **os.environ,
            "APLEXER_RUNTIME_DIR": str(runtime),
            "APLEXER_STATE_DIR": str(state),
            "APLEXER_CONFIG": str(root / "config.toml"),
        }
        ids, process = [], None

        def launch(tag):
            output = subprocess.run(
                [binary, "start", "--workspace", str(root), "--tag", tag,
                 "--engine", "shell", "--json", "--", "/bin/cat"],
                env=env, capture_output=True, check=True,
            )
            ident = json.loads(output.stdout)["id"]
            ids.append(ident)
            return ident

        try:
            ident = launch("bench")
            template = json.loads((state / "sessions" / ident / "session.json").read_text())
            for i in range(args.sessions - 1):
                if args.live:
                    launch(f"bench-{i}")
                    continue
                sid = str(uuid.uuid4())
                record = {
                    **template, "id": sid, "tag": f"bench-{i}", "phase": "exited",
                    "worker_pid": None, "workload_pid": None,
                    "socket_path": str(runtime / "sessions" / sid / "control.sock"),
                    "history_path": str(state / "sessions" / sid / "history.bin"),
                    "created_at_ms": template["created_at_ms"] - i - 1,
                }
                folder = state / "sessions" / sid
                folder.mkdir()
                (folder / "session.json").write_text(json.dumps(record))
                (folder / "history.bin").write_bytes(b"fixture\n")
            process = subprocess.Popen(
                [binary, "rpc", "--stdio", "--protocol", "1"], env=env,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            )
            sequence = 0

            def call(method, params):
                nonlocal sequence
                sequence += 1
                payload = json.dumps({
                    "protocol": 1, "id": str(sequence), "method": method, "params": params,
                }).encode()
                started = time.perf_counter_ns()
                process.stdin.write(struct.pack("!I", len(payload)) + payload)
                process.stdin.flush()
                size = struct.unpack("!I", read_exact(process.stdout, 4))[0]
                if size > 16 * 1024 * 1024:
                    raise RuntimeError("oversized helper response")
                reply = json.loads(read_exact(process.stdout, size))
                elapsed = (time.perf_counter_ns() - started) / 1e6
                if "error" in reply:
                    raise RuntimeError(reply["error"])
                return elapsed, reply

            ready, _ = call("capabilities", {})
            call("send", {"selector": ident, "payload": "z" * 16384 + "\n", "submit": False})
            seed_deadline = time.monotonic() + 2
            while True:
                _, seed = call("capture", {"selector": ident, "maxBytes": 16384})
                if seed["result"]["bytes"] == 16384:
                    break
                if time.monotonic() > seed_deadline:
                    raise RuntimeError("capture seed did not reach 16 KiB")
                time.sleep(.005)
            cases = [
                ("ping", {}), ("list", {}), ("status", {"selector": ident}),
                ("capture", {"selector": ident, "maxBytes": 16384}),
                ("send", {"selector": ident, "payload": "x", "submit": False}),
                ("listPsc", {"format": "psc"}),
            ]
            results = {}
            for label, params in cases:
                method = "list" if label == "listPsc" else label
                for _ in range(20):
                    call(method, params)
                values = []
                for _ in range(args.runs):
                    elapsed, reply = call(method, params)
                    values.append(elapsed)
                    if label == "list":
                        assert len(reply["result"]) == args.sessions
                    if label == "capture":
                        assert len(base64.b64decode(reply["result"]["data"])) == 16384
                results[label] = quantiles(values)
            visibility = []
            visibility_id = ident if args.live else next(
                p.name for p in (state / "sessions").iterdir() if p.name != ident
            )
            folder = state / "sessions" / visibility_id
            for i in range(100):
                started = time.perf_counter_ns()
                if args.live:
                    subprocess.run(
                        [binary, "rename", ident, "--tag", f"change-{i}"],
                        env=env, capture_output=True, check=True,
                    )
                else:
                    record = json.loads((folder / "session.json").read_text())
                    record["tag"] = f"change-{i}"
                    temporary = folder / "session.json.tmp"
                    temporary.write_text(json.dumps(record))
                    started = time.perf_counter_ns()
                    temporary.replace(folder / "session.json")
                while True:
                    _, reply = call("list", {})
                    if any(row["id"] == visibility_id and row["tag"] == f"change-{i}"
                           for row in reply["result"]):
                        break
                    if time.perf_counter_ns() - started > 2_000_000_000:
                        raise RuntimeError("visibility exceeded 2 seconds")
                    time.sleep(.001)
                visibility.append((time.perf_counter_ns() - started) / 1e6)
            process.stdin.close()
            process.wait(timeout=10)
            print(json.dumps({
                "binary": binary, "sha256": binary_hash,
                "loadStart": load_start, "loadEnd": os.getloadavg(),
                "sessions": args.sessions, "runs": args.runs,
                "liveSessions": args.sessions if args.live else 1,
                "captureBytes": 16384, "sendBytes": 1,
                "readyMs": round(ready, 3), "requestMs": results,
                "visibilityMs": quantiles(visibility),
            }, indent=2))
        finally:
            if process is not None and process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
            for sid in ids:
                subprocess.run([binary, "kill", sid, "--grace-ms", "0"],
                               env=env, capture_output=True)


if __name__ == "__main__":
    main()
