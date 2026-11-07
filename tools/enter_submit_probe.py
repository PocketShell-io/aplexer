#!/usr/bin/env python3
"""Cheap Enter-timing probe for `a send` submissions: no LLM reply needed.

Ground truth is the composer state ~2s after Enter, read from
`a capture --screen --plain`:
  - composer still holds the payload (or the engine's paste marker) = Enter
    was swallowed as a linebreak (FAIL);
  - composer empty/placeholder = Enter submitted the turn (OK). A turn that
    submitted is cancelled at once with Escape to keep API cost near zero.

Two modes per trial:
  - `raw`: `a send --stdin` (no enter), sleep D ms, then `a send --hex 0d`.
    This is the unframed split send; on codex it shows the failure mode the
    shipped path exists to avoid (large pastes swallow Enter at any delay).
  - `shipped`: `a send --stdin --enter --no-ack` (one RPC: explicit
    bracketed paste + configured `enter_delay_ms`/`enter_settle`, single
    Enter, no ack retries). Rewrites the isolated config between cells to
    sweep delays.

Needs nothing but the target CLIs; each engine runs against its own
isolated APLEXER_STATE_DIR / APLEXER_RUNTIME_DIR. `a send`/`a capture`
need no sender identity, so running inside an aplexer session is fine.

Usage: python3 tools/enter_submit_probe.py --bin /path/to/aplexer \
           --work /tmp/esp --engines codex,opencode --mode shipped \
           --delays 0,200,400 --reps 3 --sizes short,big3k
"""
import argparse
import os
import re
import subprocess
import time
import uuid

ENGINE_DEFS = {
    "codex": {
        "command": '["/home/alexey/.nvm/versions/node/v24.13.1/bin/codex", "-c", "check_for_update_on_startup=false", "-m", "gpt-6-luna", "-c", "model_reasoning_effort=low"]',
        "skip": '["--dangerously-bypass-approvals-and-sandbox"]',
    },
    "opencode": {
        # No --model flag on the top-level TUI (only `opencode run` has it);
        # the model travels via `a start --model` (OPENCODE_CONFIG_CONTENT).
        "command": '["/home/alexey/.nvm/versions/node/v24.13.1/bin/opencode"]',
        "skip": '["--auto"]',
        "model": "opencode/muse-spark-1.3-contributor-free",
    },
    "claude": {
        "command": '["/home/alexey/.nvm/versions/node/v24.13.1/bin/claude", "--model", "haiku"]',
        "skip": '["--dangerously-skip-permissions"]',
    },
}

FILLER = "The quick brown fox jumps over lazy dog pack my box with five dozen liquor jugs. "


def payload_for(marker, size):
    if size == "short":
        return f"{marker} ping - reply hi."
    if size == "multi":
        lines = [f"{marker} block:"]
        for i in range(12):
            lines.append(f"line {i:02d} lorem ipsum {i * 7}")
            lines.append("")
        lines.append("end.")
        return "\n".join(lines)
    return ((marker + " ") + FILLER * (3072 // len(FILLER) + 1))[:3072]


def strip(s):
    return re.sub(r"\s+", "", s)


def codex_composer(screen):
    lines = screen.splitlines()
    start = None
    for i in range(len(lines) - 1, -1, -1):
        if lines[i].lstrip().startswith("›"):
            start = i
            break
    if start is None:
        return None
    end = None
    for i in range(start + 1, len(lines)):
        line = lines[i]
        if ("Context " in line or "context left" in line or "? for shortcuts" in line
                or (i > 0 and not lines[i - 1].strip()
                    and " · " in line and line.startswith(" "))):
            end = i
            break
    if end is None:
        return None
    text = "\n".join(lines[start:end]).lstrip().lstrip("›").strip()
    if not strip(text) or text in ("Ask Codex to do anything",) or text.startswith("Implement{feature}"):
        return ("empty", "")
    return ("holds", strip(text))


def opencode_composer(screen):
    lines = screen.splitlines()
    bottom = None
    for i in range(len(lines) - 1, -1, -1):
        if lines[i].lstrip().startswith("╹▀"):
            bottom = i
            break
    if bottom is None:
        return None
    start = bottom
    while start > 0 and lines[start - 1].lstrip().startswith("┃"):
        start -= 1
    if bottom < start + 2:
        return None
    text = "\n".join(l.lstrip().lstrip("┃").strip() for l in lines[start:bottom - 1])
    if not strip(text) or text.startswith("Ask anything"):
        return ("empty", "")
    return ("holds", strip(text))


def claude_composer(screen):
    lines = screen.splitlines()
    start = None
    for i in range(len(lines) - 1, -1, -1):
        if lines[i].lstrip().startswith("❯"):
            start = i
            break
    if start is None or start == 0:
        return None
    if not lines[start - 1].strip().startswith("─"):
        return None
    end = None
    for i in range(start + 1, len(lines)):
        if lines[i].strip().startswith("─"):
            end = i
            break
    if end is None:
        return None
    text = "\n".join(lines[start:end]).lstrip().lstrip("❯").strip()
    if not strip(text) or text.startswith('Try"'):
        return ("empty", "")
    return ("holds", strip(text))


def composer(screen, engine):
    return {"codex": codex_composer, "opencode": opencode_composer,
            "claude": claude_composer}[engine](screen)


def holds_marker(comp, marker):
    if comp is None or comp[0] == "empty":
        return False
    content = comp[1]
    return ("PastedContent" in content or "Pastedtext#" in content
            or "Pasted~" in content or strip(marker[:16]) in content
            or strip(marker) in content)


def engines_config(delay=None, settle=None, only=None):
    blocks = []
    for name, spec in ENGINE_DEFS.items():
        block = (f"[engines.{name}]\ncommand = {spec['command']}\n"
                 f"skip_permissions_argv = {spec['skip']}\n")
        if name == only and (delay is not None or settle is not None):
            if delay is not None:
                block += f"enter_delay_ms = {delay}\n"
            if settle is not None:
                block += f"enter_settle = {'true' if settle else 'false'}\n"
        blocks.append(block)
    return "\n".join(blocks) + "\n"


class Engine:
    def __init__(self, root, name, bin):
        self.name = name
        self.bin = bin
        self.work = os.path.join(root, "ws-" + name)
        self.state = os.path.join(root, "state-" + name)
        self.runtime = os.path.join(root, "runtime-" + name)
        self.config = os.path.join(root, f"eng-{name}.toml")
        for path in (self.work, self.state, self.runtime):
            os.makedirs(path, exist_ok=True)
        open(self.config, "w").write(engines_config())
        self.env = {
            "HOME": "/home/alexey", "TERM": "xterm-256color",
            "PATH": os.path.dirname(bin) + ":/home/alexey/.nvm/versions/node/v24.13.1/bin:/usr/local/bin:/usr/bin:/bin",
            "APLEXER_STATE_DIR": self.state,
            "APLEXER_RUNTIME_DIR": self.runtime,
            "APLEXER_CONFIG": self.config,
            "APLEXER_BIN": bin,
        }
        self.tag = f"esp-{name}"

    def run(self, argv, timeout=60, input=None):
        return subprocess.run([self.bin] + argv, env=self.env, cwd=self.work,
                              timeout=timeout, capture_output=True, text=True,
                              input=input)

    def sel(self):
        return ["--workspace", self.work, "--tag", self.tag]

    def screen(self):
        try:
            out = self.run(["capture"] + self.sel() + ["--screen", "--plain"],
                           timeout=30)
            return out.stdout if out.returncode == 0 else ""
        except Exception:
            return ""

    def start(self):
        cmd = ["start", "--workspace", self.work, "--tag", self.tag,
               "--engine", self.name]
        if ENGINE_DEFS[self.name].get("model"):
            cmd += ["--model", ENGINE_DEFS[self.name]["model"]]
        out = self.run(cmd, timeout=120)
        if out.returncode != 0:
            raise RuntimeError(f"start failed: {out.stderr.strip()[:300]}")
        time.sleep(8)
        prev = self.screen()
        for _ in range(30):
            time.sleep(1.0)
            cur = self.screen()
            if cur and cur == prev:
                break
            prev = cur
        if "trust this folder" in prev or "rust the contents" in prev:
            self.run(["send"] + self.sel() + ["--hex", "0d"], timeout=30)
            time.sleep(3)
        self.run(["send"] + self.sel() + ["--hex", "1b"], timeout=30)
        time.sleep(1)

    def wait_idle(self, timeout=40):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            screen = self.screen()
            if ("esc to interrupt" not in screen
                    and "esc interrupt" not in screen
                    and composer(screen, self.name) is not None):
                return screen
            if "esc to interrupt" in screen or "esc interrupt" in screen:
                self.run(["send"] + self.sel() + ["--hex", "1b"], timeout=30)
                time.sleep(2)
            else:
                time.sleep(1.0)
        return self.screen()

    def clear(self):
        for _ in range(3):
            self.run(["send"] + self.sel() + ["--hex", "15"], timeout=30)
            time.sleep(0.4)
        self.run(["send"] + self.sel() + ["--hex", "1b"], timeout=30)
        time.sleep(0.5)

    def cancel_turn(self):
        self.run(["send"] + self.sel() + ["--hex", "1b"], timeout=30)
        time.sleep(1.5)
        self.clear()


def trial_raw(engine, delay_ms, size):
    marker = f"ESP{uuid.uuid4().hex[:6].upper()}"
    text = payload_for(marker, size)
    engine.wait_idle()
    engine.clear()
    sent = engine.run(["send"] + engine.sel() + ["--stdin"], input=text,
                      timeout=60)
    if sent.returncode != 0:
        return {"ok": None, "error": "text send failed: " + sent.stderr[:150]}
    time.sleep(delay_ms / 1000.0)
    entered = engine.run(["send"] + engine.sel() + ["--hex", "0d"], timeout=30)
    if entered.returncode != 0:
        return {"ok": None, "error": "enter failed: " + entered.stderr[:150]}
    time.sleep(2.0)
    screen = engine.screen()
    comp = composer(screen, engine.name)
    busy = "esc to interrupt" in screen or "esc interrupt" in screen
    if comp is None:
        rec = {"ok": None, "state": "unknown-layout", "busy": busy}
    elif holds_marker(comp, marker):
        rec = {"ok": False, "state": "holds-payload", "busy": busy}
    else:
        rec = {"ok": True,
               "state": "empty" if comp[0] == "empty" else "holds-other",
               "busy": busy}
    engine.cancel_turn() if rec.get("ok") else engine.clear()
    if busy and not rec.get("ok"):
        engine.cancel_turn()
    time.sleep(1.0)
    return rec


def trial_shipped(engine, size, delay, settle):
    with open(engine.config, "w") as handle:
        handle.write(engines_config(delay, settle, engine.name))
    marker = f"ESP{uuid.uuid4().hex[:6].upper()}"
    text = payload_for(marker, size)
    engine.wait_idle()
    engine.clear()
    sent = engine.run(["send"] + engine.sel() + ["--stdin", "--enter",
                                                 "--no-ack"],
                      input=text, timeout=90)
    if sent.returncode != 0:
        return {"ok": None, "error": "send --enter failed: " + sent.stderr[:200]}
    time.sleep(2.0)
    screen = engine.screen()
    comp = composer(screen, engine.name)
    busy = "esc to interrupt" in screen or "esc interrupt" in screen
    if comp is None:
        rec = {"ok": None, "state": "unknown-layout", "busy": busy}
    elif holds_marker(comp, marker):
        rec = {"ok": False, "state": "holds-payload", "busy": busy}
    else:
        rec = {"ok": True,
               "state": "empty" if comp[0] == "empty" else "holds-other",
               "busy": busy}
    engine.cancel_turn() if rec.get("ok") else engine.clear()
    if busy and not rec.get("ok"):
        engine.cancel_turn()
    time.sleep(1.0)
    return rec


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True)
    parser.add_argument("--work", required=True)
    parser.add_argument("--engines", default="codex,opencode")
    parser.add_argument("--mode", choices=["raw", "shipped"], default="shipped")
    parser.add_argument("--delays", default="0,200,400")
    parser.add_argument("--settle", action="store_true",
                        help="shipped mode: enable enter_settle for the cells")
    parser.add_argument("--reps", type=int, default=3)
    parser.add_argument("--sizes", default="short,big3k")
    args = parser.parse_args()
    delays = [int(part) for part in args.delays.split(",")]
    for name in args.engines.split(","):
        engine = Engine(args.work, name, args.bin)
        try:
            engine.start()
        except Exception as exc:
            print(f"[{name}] start failed: {exc}", flush=True)
            continue
        print(f"[{name}] session ready", flush=True)
        for size in args.sizes.split(","):
            for delay in delays:
                wins = 0
                states = []
                for _ in range(args.reps):
                    if args.mode == "raw":
                        rec = trial_raw(engine, delay, size)
                    else:
                        rec = trial_shipped(engine, size, delay, args.settle)
                    if rec.get("ok"):
                        wins += 1
                    states.append(rec.get("state", "?")
                                  + ("/busy" if rec.get("busy") else "")
                                  + ("/ERR" if rec.get("error") else ""))
                    print(f"[{name}] mode={args.mode} size={size} delay={delay} "
                          f"ok={rec.get('ok')} {rec}", flush=True)
                print(f"[{name}] RESULT mode={args.mode} size={size} "
                      f"delay={delay} settle={args.settle}: {wins}/{args.reps} "
                      + ",".join(states), flush=True)
        try:
            engine.run(["kill"] + engine.sel(), timeout=60)
        except Exception:
            pass


if __name__ == "__main__":
    main()
