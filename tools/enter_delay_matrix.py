#!/usr/bin/env python3
"""Per-engine Enter-delay matrix for `a send` submissions.

For each engine, measures the minimum wall-clock gap between pasting text
into the agent TUI and sending the Enter keystroke for the message to
reliably submit. Trials send the text via `a send --stdin` (no enter),
sleep D ms, then send the Enter keystroke as a raw byte (`--hex 0d`), and
detect success from the model's reply appearing on the captured screen —
the user-level ground truth "the message was submitted and processed".

Also validates the built-in `enter_settle`/`enter_delay_ms` submission
path (`a send --enter`) end-to-end by rewriting the isolated config
between phases: at delay 0 without settle the big-paste failure mode
should reproduce; with settle it should not.

Runs engines concurrently, one thread each, every engine against its own
isolated APLEXER_STATE_DIR / APLEXER_RUNTIME_DIR so nothing touches a
live aplexer installation.

Usage: python3 tools/enter_delay_matrix.py --bin /path/to/aplexer \
           --work /tmp/etd --out /tmp/etd/results [--reps 3] [--quick]
"""
import argparse, json, os, random, re, subprocess, sys, threading, time, uuid

# Engine ids are the real family ids on purpose: aplexer keys family
# behavior (workspace-trust preparation, codex bracketed-paste framing,
# per-family enter-delay defaults) off the engine id, so the matrix must
# exercise the same ids production sends use.
ENGINES = ["codex", "zcodex", "claude", "opencode", "antigravity"]
ENGINE_DEFS = {
    "codex": {
        "command": '["/home/alexey/.nvm/versions/node/v24.13.1/bin/codex", "-c", "check_for_update_on_startup=false", "-m", "gpt-6-luna", "-c", "model_reasoning_effort=low"]',
        "skip": '["--dangerously-bypass-approvals-and-sandbox"]',
    },
    "zcodex": {
        "command": '["/home/alexey/.local/bin/zcodex", "-c", "check_for_update_on_startup=false"]',
        "skip": '["--dangerously-bypass-approvals-and-sandbox"]',
    },
    "claude": {
        "command": '["/home/alexey/.nvm/versions/node/v24.13.1/bin/claude", "--model", "haiku"]',
        "skip": '["--dangerously-skip-permissions"]',
    },
    "opencode": {
        "command": '["/home/alexey/.nvm/versions/node/v24.13.1/bin/opencode", "--model", "opencode/muse-spark-1.3-contributor-free"]',
        "skip": '["--auto"]',
    },
    "antigravity": {
        "command": '["/home/alexey/.local/bin/agy", "--model", "gemini-3.8-flash-high"]',
        "skip": '["--dangerously-skip-permissions"]',
    },
}

def engines_config(extra=None):
    """The whole isolated config; `extra` maps engine id -> additional
    fields spliced INTO that engine's table (TOML tables cannot be
    redefined append-only)."""
    extra = extra or {}
    blocks = []
    for name, spec in ENGINE_DEFS.items():
        block = (f"[engines.{name}]\n"
                 f"command = {spec['command']}\n"
                 f"skip_permissions_argv = {spec['skip']}\n")
        if name in extra:
            block += extra[name].rstrip() + "\n"
        blocks.append(block)
    return "\n".join(blocks) + "\n"
REPLY = "ACKDONE"
REPLY_RE = re.compile(r"^\W*" + REPLY + r"\W*$", re.M)
DELAYS_MS = [0, 50, 100, 200, 400, 800]
SIZES = ["short", "multi", "big3k"]

FILLER = ("The quick brown fox jumps over the lazy dog while reviewers "
          "argue about naming. Paste bursts drain through key event queues, "
          "and an Enter landing mid-drain is swallowed by the composer. ")

def payload_for(marker, size):
    if size == "short":
        return f"{marker} single-line ping. Reply with the single word {REPLY}."
    if size == "multi":
        lines = [f"{marker} multi-line block:"]
        for i in range(12):
            lines.append(f"line {i:02d}: lorem ipsum readiness window {i * 7} ms")
            lines.append("")
        lines.append(f"End of block. Reply with the single word {REPLY}.")
        return "\n".join(lines)
    body = (marker + " ") + FILLER * (3072 // len(FILLER) + 1)
    return body[:3072] + f" Reply with the single word {REPLY}."

class Engine:
    def __init__(self, root, name, bin, args):
        self.name = name
        self.bin = bin
        self.reps = args.reps
        self.enter_only = getattr(args, "enter_only", False)
        self.reply_timeout = args.reply_timeout
        self.work = os.path.join(root, "ws-" + name)
        self.state = os.path.join(root, "state-" + name)
        self.runtime = os.path.join(root, "runtime-" + name)
        self.config = os.path.join(root, f"engines-{name}.toml")
        self.results = []
        self.lock = threading.Lock()
        os.makedirs(self.work, exist_ok=True)
        os.makedirs(self.state, exist_ok=True)
        os.makedirs(self.runtime, exist_ok=True)
        with open(self.config, "w") as f:
            f.write(engines_config())
        self.env = {
            "HOME": "/home/alexey", "TERM": "xterm-256color",
            "PATH": os.path.dirname(bin) + ":/home/alexey/.nvm/versions/node/v24.13.1/bin:/usr/local/bin:/usr/bin:/bin",
            "APLEXER_STATE_DIR": self.state,
            "APLEXER_RUNTIME_DIR": self.runtime,
            "APLEXER_CONFIG": self.config,
            "APLEXER_BIN": bin,
        }
        self.suffix = 1
        self.tag = None

    def run(self, argv, timeout=60, input=None):
        return subprocess.run([self.bin] + argv, env=self.env, cwd=self.work,
                              timeout=timeout, capture_output=True, text=True,
                              input=input)

    def sel(self):
        return ["--workspace", self.work, "--tag", self.tag]

    def screen(self):
        out = self.run(["capture"] + self.sel() + ["--screen", "--plain"], timeout=30)
        return out.stdout if out.returncode == 0 else ""

    def start_session(self):
        self.tag = f"etd-{self.name}-{self.suffix}"
        out = self.run(["start", "--workspace", self.work, "--tag", self.tag,
                        "--engine", self.name], timeout=120)
        if out.returncode != 0:
            raise RuntimeError(f"start failed: {out.stderr.strip()[:300]}")
        # Wait for the TUI to settle: two consecutive identical screens.
        time.sleep(8)
        prev = self.screen()
        for _ in range(30):
            time.sleep(1.0)
            cur = self.screen()
            if cur and cur == prev:
                break
            prev = cur
        # A workspace-trust dialog is a static screen that settles; accept
        # it (arrow keys first when the preselection is the exit option).
        if "trust this folder" in prev or "rust the contents" in prev:
            no_selected = ("No, exit" in prev.split("enter Confirm")[0].split(">")[-1]
                           or "> No" in prev or "❯ No" in prev)
            if no_selected:
                self.run(["send"] + self.sel() + ["--hex", "1b5b42"], timeout=30)
                time.sleep(0.5)
            self.run(["send"] + self.sel() + ["--hex", "0d"], timeout=30)
            time.sleep(3)
            prev = self.screen()
            for _ in range(20):
                time.sleep(1.0)
                cur = self.screen()
                if cur and cur == prev:
                    return
                prev = cur
        else:
            # Dismiss first-run promo overlays (agy's "esc to dismiss")
            # before the first trial; harmless on an empty composer.
            self.run(["send"] + self.sel() + ["--hex", "1b"], timeout=30)
            time.sleep(2)
            prev = self.screen()
            for _ in range(20):
                time.sleep(1.0)
                cur = self.screen()
                if cur and cur == prev:
                    return
                prev = cur
        print(f"[{self.name}] boot settle best-effort exhausted; proceeding",
              flush=True)

    def restart_if_broken(self, marker):
        # Composer still holds the marker: try Ctrl-U clears, then restart.
        for _ in range(2):
            self.run(["send"] + self.sel() + ["--hex", "15"], timeout=30)
            time.sleep(0.5)
        if marker not in self.screen():
            return True
        self.kill()
        self.suffix += 1
        self.start_session()
        return marker not in self.screen()

    def kill(self):
        try:
            self.run(["kill"] + self.sel(), timeout=60)
        except subprocess.TimeoutExpired:
            pass
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            out = self.run(["list", "--json"], timeout=20)
            rows = json.loads(out.stdout or "[]")
            if not any(r["tag"] == self.tag for r in rows):
                return
            time.sleep(0.5)

    def ensure_session(self):
        out = self.run(["list", "--json"], timeout=20)
        rows = json.loads(out.stdout or "[]")
        if any(r["tag"] == self.tag for r in rows):
            return
        self.suffix += 1
        self.start_session()

    def trial(self, delay_ms, size, rep, enter_mode=False):
        self.ensure_session()
        marker = f"ETD{self.name[:4].upper()}D{delay_ms:03d}{size[:2].upper()}{rep}-{uuid.uuid4().hex[:4]}"
        reply = REPLY + marker
        reply_re = re.compile(r"^\W*" + re.escape(reply) + r"\W*$", re.M)
        text = "Dry run. Do not use tools or contact external services. " + payload_for(marker, size).replace(REPLY, reply)
        rec = {"engine": self.name, "delay_ms": delay_ms, "size": size,
               "rep": rep, "marker": marker, "mode": enter_mode}
        try:
            t0 = time.monotonic()
            if enter_mode:
                r = self.run(["send"] + self.sel() + ["--stdin", "--enter"],
                             input=text, timeout=60)
                rec["mode"] = "enter-path"
            else:
                r = self.run(["send"] + self.sel() + ["--stdin"], input=text, timeout=60)
            if r.returncode != 0:
                rec["error"] = f"text send failed: {r.stderr.strip()[:200]}"
                return rec
            if not enter_mode:
                time.sleep(delay_ms / 1000.0)
                r = self.run(["send"] + self.sel() + ["--hex", "0d"], timeout=60)
                if r.returncode != 0:
                    rec["error"] = f"enter send failed: {r.stderr.strip()[:200]}"
                    return rec
            deadline = time.monotonic() + self.reply_timeout
            while time.monotonic() < deadline:
                if reply_re.search(self.screen()):
                    rec["ok"] = True
                    rec["reply_after_s"] = round(time.monotonic() - t0, 2)
                    break
                time.sleep(0.4)
            else:
                rec["ok"] = False
                screen = self.screen()
                rec["marker_on_screen"] = marker in screen
                rec["screen_tail"] = screen[-400:]
            # Settle between trials; clean a failed composer.
            time.sleep(2.5)
            if not rec.get("ok"):
                rec["cleaned"] = self.restart_if_broken(marker)
            return rec
        except (subprocess.TimeoutExpired, RuntimeError) as exc:
            rec["error"] = str(exc)[:300]
            return rec

    def run_matrix(self):
        self.start_session()
        cells = [(d, s, r) for d in ([0] if self.enter_only else DELAYS_MS) for s in SIZES for r in range(self.reps)]
        random.Random(20261014).shuffle(cells)
        for delay, size, rep in cells:
            rec = self.trial(delay, size, rep, enter_mode=self.enter_only)
            with self.lock:
                self.results.append(rec)
                print(f"[{self.name}] delay={delay:4d} size={size:6s} rep={rep} "
                      f"ok={rec.get('ok')} {rec.get('error','')}", flush=True)
        self.kill()

    def run_enter_path(self, label, engine_fields):
        """Trials through the shipped `a send --enter` path with a given
        per-engine config; the isolated config is rewritten whole (a TOML
        table cannot be redefined append-only)."""
        with open(self.config, "w") as f:
            f.write(engines_config({self.name: engine_fields}))
        out = []
        for rep in range(3):
            rec = self.trial(0, "big3k", rep, enter_mode=True)
            rec["label"] = label
            out.append(rec)
            print(f"[{self.name}] enter-path {label} rep={rep} ok={rec.get('ok')}", flush=True)
        with open(self.config, "w") as f:
            f.write(engines_config())
        return out


def summarize(results):
    by_engine = {}
    for r in results:
        by_engine.setdefault(r["engine"], []).append(r)
    summary = {}
    for eng, rows in sorted(by_engine.items()):
        per = {}
        for r in rows:
            if r.get("mode") == "enter-path":
                continue
            key = (r["delay_ms"], r["size"])
            per.setdefault(key, []).append(1 if r.get("ok") else 0)
        # Minimal delay with 100% across all sizes and reps.
        reliable = None
        for d in DELAYS_MS:
            cells = [v for (dd, _), vals in per.items() if dd == d for v in vals]
            if cells and all(cells):
                reliable = d
                break
        grid = {f"{d}ms": {s: f"{sum(per.get((d, s), []))}/{len(per.get((d, s), []))}" for s in SIZES} for d in DELAYS_MS}
        enter = [r for r in rows if r.get("mode") == "enter-path"]
        summary[eng] = {"grid": grid, "min_reliable_delay_ms": reliable,
                        "enter_path": {"passed": sum(bool(r.get("ok")) for r in enter), "total": len(enter)}}
    return summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--reply-timeout", type=int, default=90)
    ap.add_argument("--enter-only", action="store_true", help="validate real --enter submissions at configured defaults")
    ap.add_argument("--quick", action="store_true", help="reps=1, subset")
    args = ap.parse_args()
    if args.quick:
        args.reps = 1
    random.seed(20261014)
    names = os.environ.get("ETD_ENGINES", ",".join(ENGINES)).split(",")
    threads, engines = [], []
    for name in names:
        e = Engine(args.work, name, args.bin, args)
        engines.append(e)
        t = threading.Thread(target=e.run_matrix)
        t.start()
        threads.append(t)
    for t in threads:
        t.join(timeout=3600)
    results = [r for e in engines for r in e.results]
    summary = summarize(results)
    with open(args.out + ".json", "w") as f:
        json.dump({"summary": summary, "trials": results}, f, indent=1)
    print(json.dumps(summary, indent=1), flush=True)
    if args.enter_only and (not results or any(not r.get("ok") for r in results)):
        raise SystemExit(1)

if __name__ == "__main__":
    main()
