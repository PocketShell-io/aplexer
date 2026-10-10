#!/usr/bin/env python3
"""Administrative rollout wrapper: candidate 53f535a2 -> six installed aplexer ELFs.

Frozen administrative fix per ROOT review of docs/rollout-53f535a2-proposal.md
rev 2. Not a product code path: this file only orchestrates a verified, atomic,
per-target replace of the six named ELF directory entries. It never touches
Python shims, hook configs, or the running supervisor/workers, and it never
opens a target for writing -- replacement is rename(2) over the directory
entry only, so hardlink siblings of a replaced inode keep the old inode and
content untouched.

Subcommands:
  preflight            READ-ONLY: verify every pin, snapshot workers, hook
                       configs and hardlink siblings; write a receipt under
                       RECEIPT_ROOT. No filesystem writes outside receipts.
  apply                preflight again, then per target: same-dir backup with
                       the original bytes and 0775, same-dir tempfile created
                       O_EXCL|O_NOFOLLOW with the candidate bytes and 0755,
                       full-hash+mode verify, atomic rename. STOPS at the
                       first discrepancy and rolls back only this run's
                       committed targets that are still at the candidate SHA.
  rollback --manifest  restore committed targets that are still at the
                       candidate SHA from their verified original backups;
                       a target whose content changed concurrently is left
                       alone and reported.
"""

import argparse
import glob
import hashlib
import json
import os
import stat
import subprocess
import sys
import time
import uuid

CANDIDATE = "/home/alexey/.aplexer-fix-gated-idle/bin/aplexer"
CANDIDATE_SHA = "53f535a2512f3572e61b4a0058e2506ddaa8a25186cb89b5faf17afc3aceba02"
EXPECTED_UID = 1000
NEW_MODE = 0o755
RECEIPT_ROOT = "/home/alexey/.aplexer-rollout-53f535a2"

T3_SHA = "0a4e6893e22429867c8d396711ecb526ad4e506627f3889dd7f6ee8969ff25ac"
TARGETS = [
    dict(name="T1", path="/home/alexey/.local/bin/aplexer",
         orig_sha="5655521e4881ad8e9d283434027361c0905f2f04e11a55c26ed7c473a3f1ad48",
         inode=6314299, nlink=1),
    dict(name="T2",
         path="/home/alexey/.local/share/uv/tools/pocketshell/lib/python3.14/site-packages/aplexer_cli/bin/aplexer",
         orig_sha="790dfc6446a709499cbdf02a36d04eb3224eb352e55cbefd7937888e7cb69eb1",
         inode=16254705, nlink=1),
    dict(name="T3a", path="/home/alexey/git/pocketshell-cli/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer",
         orig_sha=T3_SHA, inode=11944740, nlink=1),
    dict(name="T3b", path="/home/alexey/git/pocketshell-cli-presence/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer",
         orig_sha=T3_SHA, inode=14332318, nlink=5),
    dict(name="T3c", path="/home/alexey/git/pocketshell-cli-gateway-service/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer",
         orig_sha=T3_SHA, inode=14332318, nlink=5),
    dict(name="T3d", path="/home/alexey/git/pocketshell-cli-windows-gateway/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer",
         orig_sha=T3_SHA, inode=14332318, nlink=5),
]

SWEEP_ROOTS = ["/home/alexey/git", "/home/alexey/.local/share/uv", "/home/alexey/.cache/uv"]
SWEEP_SECONDS = 25.0


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb", buffering=0) as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def lstat_or_none(path):
    try:
        return os.lstat(path)
    except OSError:
        return None


def parse_probe():
    """Read-only: the staged candidate must refuse `gated-idle` past clap with
    the engine-scope message (exit 1), proving the parse fix, with
    APLEXER_SESSION_ID unset so nothing is written to any state store."""
    env = dict(os.environ)
    env.pop("APLEXER_SESSION_ID", None)
    r = subprocess.run([CANDIDATE, "state-report", "gated-idle"],
                       stdin=subprocess.DEVNULL, capture_output=True, env=env)
    return r.returncode, r.stderr.decode(errors="replace").strip()


def snapshot_workers():
    """Every live process whose exe image mentions aplexer: pid, kernel start
    time (birth), exe target, plus a count of aplexer-related mappings."""
    out = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        exe = os.readlink("/proc/%s/exe" % name) if os.path.exists("/proc/%s/exe" % name) else None
        if not exe or "aplexer" not in exe:
            continue
        try:
            with open("/proc/%s/stat" % name) as f:
                raw = f.read()
            starttime = raw[raw.rindex(")") + 2:].split()[19]
        except (OSError, ValueError):
            starttime = None
        maps_hits = None
        try:
            with open("/proc/%s/maps" % name) as f:
                maps_hits = sum(1 for line in f if "aplexer" in line)
        except OSError:
            pass
        out[int(name)] = dict(starttime=starttime, exe=exe, aplexer_map_lines=maps_hits)
    return dict(sorted(out.items()))


def snapshot_hooks():
    """Hash every hook config that references state-report (must not change)."""
    files = ["/home/alexey/.claude/settings.json"]
    files += [p for p in glob.glob("/home/alexey/git/*/.claude/settings*.json")
              if os.path.isfile(p) and b"state-report" in open(p, "rb").read()]
    return {p: sha256(p) for p in sorted(set(files))}


def sweep_samefile(want_dev, want_ino, deadline):
    """Best-effort bounded enumeration of hardlink siblings of an inode."""
    found, partial, n = [], False, 0
    for root in SWEEP_ROOTS:
        if not os.path.isdir(root):
            continue
        for dirpath, dirnames, filenames in os.walk(root):
            n += 1
            if n % 256 == 0 and time.monotonic() > deadline:
                return found, True
            for f in filenames:
                p = os.path.join(dirpath, f)
                try:
                    st = os.lstat(p)
                except OSError:
                    continue
                if st.st_ino == want_ino and st.st_dev == want_dev:
                    found.append(p)
    return found, partial


def check_target(t, problems, prefix):
    st = lstat_or_none(t["path"])
    if st is None:
        problems.append("%s: missing" % prefix)
        return None
    if not stat.S_ISREG(st.st_mode):
        problems.append("%s: not a regular file" % prefix)
        return None
    if st.st_uid != EXPECTED_UID:
        problems.append("%s: uid %d != %d" % (prefix, st.st_uid, EXPECTED_UID))
    if stat.S_IMODE(st.st_mode) != 0o775:
        problems.append("%s: mode %o != 775" % (prefix, stat.S_IMODE(st.st_mode)))
    if st.st_ino != t["inode"]:
        problems.append("%s: inode %d != expected %d" % (prefix, st.st_ino, t["inode"]))
    if st.st_nlink != t["nlink"]:
        problems.append("%s: nlink %d != expected %d" % (prefix, st.st_nlink, t["nlink"]))
    if sha256(t["path"]) != t["orig_sha"]:
        problems.append("%s: sha mismatch vs pinned original" % prefix)
    return dict(inode=st.st_ino, nlink=st.st_nlink, mode=oct(stat.S_IMODE(st.st_mode)),
                uid=st.st_uid, size=st.st_size, dev=st.st_dev)


def preflight(run_dir):
    receipt = dict(when=time.strftime("%Y-%m-%dT%H:%M:%S%z"), nonce=uuid.uuid4().hex[:12],
                   candidate=CANDIDATE, candidate_sha=CANDIDATE_SHA, targets={}, problems=[])
    p = receipt["problems"]
    cst = lstat_or_none(CANDIDATE)
    if cst is None or not stat.S_ISREG(cst.st_mode):
        p.append("candidate missing or not regular")
    else:
        receipt["candidate_uid"] = cst.st_uid
        receipt["candidate_mode"] = oct(stat.S_IMODE(cst.st_mode))
        if cst.st_uid != EXPECTED_UID:
            p.append("candidate uid %d" % cst.st_uid)
        if sha256(CANDIDATE) != CANDIDATE_SHA:
            p.append("candidate sha mismatch")
    rc, err = parse_probe()
    receipt["parse_probe"] = dict(exit=rc, stderr=err)
    if not (rc == 1 and "wired for engine claude only" in err):
        p.append("parse probe unexpected: exit=%d %s" % (rc, err))
    for t in TARGETS:
        receipt["targets"][t["name"]] = check_target(t, p, t["name"]) or {}
    receipt["workers"] = snapshot_workers()
    receipt["worker_count"] = len(receipt["workers"])
    receipt["hooks"] = snapshot_hooks()
    st = receipt["targets"].get("T3b") or {}
    if st.get("dev") is not None and st.get("inode") == 14332318:
        links, partial = sweep_samefile(st["dev"], 14332318, time.monotonic() + SWEEP_SECONDS)
        receipt["shared_inode_14332318_links"] = dict(paths=links, sweep_partial=partial,
                                                      named=[t["path"] for t in TARGETS[3:]])
        for lp in links:
            if lp not in [t["path"] for t in TARGETS[3:]]:
                l = lstat_or_none(lp)
                if l is None or sha256(lp) != T3_SHA:
                    p.append("unnamed link already divergent: %s" % lp)
    os.makedirs(run_dir, exist_ok=True)
    j = os.path.join(run_dir, "receipt.json")
    with open(j, "w") as f:
        json.dump(receipt, f, indent=1, sort_keys=True)
    os.chmod(j, 0o644)
    with open(os.path.join(run_dir, "summary.txt"), "w") as f:
        f.write("preflight %s: %s\n" % (receipt["nonce"], "PASS" if not p else "FAIL"))
        f.write("worker_count=%d hook_files=%d\n" % (receipt["worker_count"], len(receipt["hooks"])))
        for name, s in sorted(receipt["targets"].items()):
            f.write("%s %s\n" % (name, s))
        for line in p:
            f.write("PROBLEM: %s\n" % line)
    return receipt


def copy_verified(src, dst_fd, dst_path, want_sha, want_mode, want_uid):
    h = hashlib.sha256()
    with open(src, "rb", buffering=0) as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            os.write(dst_fd, chunk)
            h.update(chunk)
    os.fsync(dst_fd)
    os.close(dst_fd)
    st = os.lstat(dst_path)
    if h.hexdigest() != want_sha:
        raise RuntimeError("copy sha mismatch %s" % dst_path)
    if stat.S_IMODE(st.st_mode) != want_mode or st.st_uid != want_uid:
        raise RuntimeError("copy meta mismatch %s" % dst_path)


def make_file_same_dir(directory, basename, suffix, nonce, mode):
    """O_EXCL|O_NOFOLLOW regular file in the target's own directory."""
    path = os.path.join(directory, ".%s%s-%s" % (basename, suffix, nonce))
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    return fd, path


def apply():
    if os.geteuid() != EXPECTED_UID:
        sys.exit("refusing: euid != %d" % EXPECTED_UID)
    nonce = uuid.uuid4().hex[:12]
    run_dir = os.path.join(RECEIPT_ROOT, "run-%s" % nonce)
    receipt = preflight(run_dir)
    if receipt["problems"]:
        print("PREFLIGHT FAIL, nothing written to targets. receipt: %s" % run_dir)
        for line in receipt["problems"]:
            print("  " + line)
        sys.exit(1)
    manifest = dict(nonce=nonce, run_dir=run_dir, started=receipt["when"],
                    workers_before=receipt["workers"], hooks_before=receipt["hooks"],
                    committed=[], done=False, status="in-progress")
    write_manifest(run_dir, manifest)
    committed = []
    try:
        for t in TARGETS:
            directory = os.path.dirname(t["path"])
            base = os.path.basename(t["path"])
            st = lstat_or_none(t["path"])
            if st is None or stat.S_IMODE(st.st_mode) != 0o775 or st.st_uid != EXPECTED_UID \
                    or sha256(t["path"]) != t["orig_sha"]:
                raise RuntimeError("pre-rename drift on %s" % t["name"])
            bfd, bpath = make_file_same_dir(directory, base, ".orig", nonce, 0o775)
            copy_verified(t["path"], bfd, bpath, t["orig_sha"], 0o775, EXPECTED_UID)
            tfd, tpath = make_file_same_dir(directory, base, ".new", nonce, NEW_MODE)
            copy_verified(CANDIDATE, tfd, tpath, CANDIDATE_SHA, NEW_MODE, EXPECTED_UID)
            new_ino = os.lstat(tpath).st_ino
            os.rename(tpath, t["path"])
            st2 = lstat_or_none(t["path"])
            if st2 is None or stat.S_IMODE(st2.st_mode) != NEW_MODE or st2.st_uid != EXPECTED_UID \
                    or st2.st_ino != new_ino or sha256(t["path"]) != CANDIDATE_SHA:
                raise RuntimeError("post-rename verify failed on %s" % t["name"])
            committed.append(dict(name=t["name"], path=t["path"], backup=bpath,
                                  orig_sha=t["orig_sha"], new_inode=new_ino))
            manifest["committed"] = committed
            write_manifest(run_dir, manifest)
            for other in TARGETS[3:]:
                if other["name"] in [c["name"] for c in committed] or other["name"] == t["name"]:
                    continue
                ost = lstat_or_none(other["path"])
                if ost is None or ost.st_ino != 14332318 or sha256(other["path"]) != T3_SHA:
                    raise RuntimeError("shared-inode sibling disturbed: %s" % other["name"])
    except Exception as exc:
        manifest["status"] = "failed"
        manifest["failure"] = str(exc)
        rolled, skipped = rollback_committed(manifest)
        manifest["rollback"] = dict(rolled=rolled, skipped=skipped)
        manifest["done"] = True
        write_manifest(run_dir, manifest)
        print("STOPPED at first discrepancy: %s" % exc)
        print("rolled back: %s; skipped (concurrent content): %s" % (rolled, skipped))
        print("receipt: %s" % run_dir)
        sys.exit(1)
    problems = post_verify(receipt, manifest)
    manifest["status"] = "success" if not problems else "post-verify-fail"
    manifest["post_verify_problems"] = problems
    manifest["done"] = True
    write_manifest(run_dir, manifest)
    if problems:
        print("APPLIED but post-verify reported problems: %s" % problems)
        print("receipt: %s" % run_dir)
        sys.exit(1)
    print("APPLIED nonce=%s receipt=%s" % (nonce, run_dir))


def post_verify(before, manifest):
    problems = []
    for t in TARGETS:
        st = lstat_or_none(t["path"])
        if st is None or stat.S_IMODE(st.st_mode) != NEW_MODE or st.st_uid != EXPECTED_UID \
                or sha256(t["path"]) != CANDIDATE_SHA:
            problems.append("%s post-state wrong" % t["name"])
    for lp in before.get("shared_inode_14332318_links", {}).get("paths", []):
        if lp in [t["path"] for t in TARGETS[3:]]:
            continue
        st = lstat_or_none(lp)
        if st is None or st.st_ino != 14332318 or sha256(lp) != T3_SHA:
            problems.append("unnamed link changed: %s" % lp)
    if snapshot_workers() != manifest["workers_before"]:
        problems.append("worker birth/exe snapshot changed")
    if snapshot_hooks() != manifest["hooks_before"]:
        problems.append("hook config hashes changed")
    return problems


def rollback_committed(manifest):
    rolled, skipped = [], []
    for c in reversed(manifest["committed"]):
        try:
            cur = sha256(c["path"])
        except OSError:
            skipped.append(dict(path=c["path"], reason="unreadable"))
            continue
        if cur != CANDIDATE_SHA:
            skipped.append(dict(path=c["path"], reason="content changed concurrently"))
            continue
        bpath = c["backup"]
        bst = lstat_or_none(bpath)
        if bst is None or stat.S_IMODE(bst.st_mode) != 0o775 or sha256(bpath) != c["orig_sha"]:
            skipped.append(dict(path=c["path"], reason="backup missing or unverified"))
            continue
        os.rename(bpath, c["path"])
        if sha256(c["path"]) != c["orig_sha"]:
            raise RuntimeError("rollback verify failed %s" % c["path"])
        rolled.append(c["path"])
    return rolled, skipped


def write_manifest(run_dir, manifest):
    p = os.path.join(run_dir, "manifest.json")
    with open(p, "w") as f:
        json.dump(manifest, f, indent=1, sort_keys=True)
    os.chmod(p, 0o644)


def rollback_cmd(manifest_path):
    with open(manifest_path) as f:
        manifest = json.load(f)
    if not manifest.get("done"):
        sys.exit("refusing: run %s not finished" % manifest["nonce"])
    rolled, skipped = rollback_committed(manifest)
    manifest["rollback"] = dict(rolled=rolled, skipped=skipped)
    write_manifest(manifest["run_dir"], manifest)
    print("rolled back: %s; skipped: %s" % (rolled, skipped))


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("command", choices=["preflight", "apply", "rollback"])
    ap.add_argument("--manifest", help="manifest.json of a finished run (rollback)")
    ap.add_argument("--run-dir", help="existing preflight receipt dir to reuse (apply)")
    a = ap.parse_args()
    os.makedirs(RECEIPT_ROOT, exist_ok=True)
    os.chmod(RECEIPT_ROOT, 0o755)
    if a.command == "preflight":
        d = a.run_dir or os.path.join(RECEIPT_ROOT, "preflight-%s" % uuid.uuid4().hex[:12])
        r = preflight(d)
        print("preflight %s: %s -> %s" % (r["nonce"], "PASS" if not r["problems"] else "FAIL", d))
        for line in r["problems"]:
            print("  PROBLEM: " + line)
        sys.exit(1 if r["problems"] else 0)
    elif a.command == "apply":
        apply()
    else:
        if not a.manifest:
            sys.exit("rollback requires --manifest")
        rollback_cmd(a.manifest)


if __name__ == "__main__":
    main()
