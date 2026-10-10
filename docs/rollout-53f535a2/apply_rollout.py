#!/usr/bin/env python3
"""Administrative rollout wrapper: candidate 53f535a2 -> six installed aplexer ELFs.

Frozen administrative fix, rev 3. Not a product code path: this file only
orchestrates a verified, atomic, per-target replace of the six named ELF
directory entries. It never touches Python shims, hook configs, or the
running supervisor/workers, and it never opens a target for writing --
replacement is rename(2) over the directory entry only, so hardlink
siblings of a replaced inode keep the old inode and content untouched.

Rev 3 corrections over the refused rev-2 baseline (9ddd315), per ROOT
journal findings (inbox 01a12347):
  - Durable journal: every manifest/receipt write creates a fresh
    O_EXCL|O_NOFOLLOW temp in the run dir, fchmods 0644, fsyncs, renames it
    over the journal atomically in the same directory, then fsyncs the
    directory. A target's INTENT record (backup pin + exact install
    identity) is durable BEFORE that target's rename(2), so an interrupted
    run always leaves backup custody and a recoverable transaction
    (rollback --manifest --force).
  - Installed-identity rollback: each committed entry records the exact
    installed identity (dev, inode, type, uid, gid, mode, candidate sha).
    Rollback restores only a target whose on-disk file still matches that
    identity AND still hashes to the candidate sha, so a concurrent
    same-sha replacement (new inode) is never clobbered; any concurrent
    change is skipped and reported.
  - Backup pins are validated by lstat at restore time: non-symlink regular
    file, recorded uid/gid, mode 0775, recorded dev/ino, full original sha.
    Any mismatch -> skip and report; never restore from an unverified
    backup.
  - Post-verify runs INSIDE the transaction try: any problem -- including
    failed /proc reads or a process disappearing mid-check -- takes the
    same first-discrepancy scoped-rollback path instead of an unhandled
    exit.
  - Hardening found while correcting: exact modes enforced with fchmod (a
    umask can mask os.open's mode argument), source reads open the target
    O_NOFOLLOW, and every created file is fstat-verified regular and owned
    before use.

Subcommands:
  preflight            READ-ONLY: verify every pin, snapshot workers, hook
                       configs and hardlink siblings; write a receipt under
                       RECEIPT_ROOT. No filesystem writes outside receipts.
  apply                preflight again, then per target: same-dir backup with
                       the original bytes and 0775, same-dir tempfile created
                       O_EXCL|O_NOFOLLOW with the candidate bytes and 0755,
                       full-hash+mode verify, durable intent journal, atomic
                       rename. STOPS at the first discrepancy (including any
                       post-verify problem) and rolls back only this run's
                       installed-identity-matched, still-candidate targets.
  rollback --manifest [--force]
                       restore committed targets that still match their
                       recorded installed identity and the candidate sha
                       from their verified original backups; anything else
                       is left alone and reported. --force recovers an
                       interrupted (not finalized) run journal.
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
ORIG_MODE = 0o775
RECEIPT_ROOT = "/home/alexey/.aplexer-rollout-53f535a2"

T3_SHA = "0a4e6893e22429867c8d396711ecb526ad4e506627f3889dd7f6ee8969ff25ac"
T3_INO = 14332318
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
         orig_sha=T3_SHA, inode=T3_INO, nlink=5),
    dict(name="T3c", path="/home/alexey/git/pocketshell-cli-gateway-service/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer",
         orig_sha=T3_SHA, inode=T3_INO, nlink=5),
    dict(name="T3d", path="/home/alexey/git/pocketshell-cli-windows-gateway/.venv/lib/python3.14/site-packages/aplexer_cli/bin/aplexer",
         orig_sha=T3_SHA, inode=T3_INO, nlink=5),
]

SWEEP_ROOTS = ["/home/alexey/git", "/home/alexey/.local/share/uv", "/home/alexey/.cache/uv"]
SWEEP_SECONDS = 25.0

FINALIZED = ("success", "failed", "interrupted", "rolled-back", "rolled-back-partial")
_journal_seq = [0]


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


def fsync_dir(path):
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def durable_write_json(run_dir, final_name, payload):
    """Atomic + durable journal write: fresh O_EXCL|O_NOFOLLOW temp in
    run_dir, fchmod 0644, fsync, same-dir rename over final_name, then
    directory fsync. Every call uses a fresh temp name so O_EXCL holds
    across the many journal updates of one run."""
    _journal_seq[0] += 1
    tmp = os.path.join(run_dir, ".journal.%s.%s.%d.tmp"
                       % (os.path.basename(final_name), payload.get("nonce", "anon"),
                          _journal_seq[0]))
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
    try:
        os.fchmod(fd, 0o644)
        with os.fdopen(fd, "w") as f:
            json.dump(payload, f, indent=1, sort_keys=True)
            f.flush()
            os.fsync(f.fileno())
        os.rename(tmp, os.path.join(run_dir, final_name))
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    fsync_dir(run_dir)


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
    time (birth), exe target, plus a count of aplexer-related mappings.
    Returns (snapshot, read_errors): a process that disappears mid-walk
    simply vanishes from the snapshot; an aplexer process whose stat or
    maps cannot be read is reported as an error, never silently skipped."""
    out, errors = {}, []
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            exe = os.readlink("/proc/%s/exe" % name)
        except OSError:
            continue
        if "aplexer" not in exe:
            continue
        starttime = None
        try:
            with open("/proc/%s/stat" % name) as f:
                raw = f.read()
            starttime = raw[raw.rindex(")") + 2:].split()[19]
        except (OSError, ValueError, IndexError):
            errors.append("pid %s stat unreadable" % name)
        maps_hits = None
        try:
            with open("/proc/%s/maps" % name) as f:
                maps_hits = sum(1 for line in f if "aplexer" in line)
        except OSError:
            errors.append("pid %s maps unreadable" % name)
        out[int(name)] = dict(starttime=starttime, exe=exe, aplexer_map_lines=maps_hits)
    return dict(sorted(out.items())), errors


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
    if stat.S_IMODE(st.st_mode) != ORIG_MODE:
        problems.append("%s: mode %o != 775" % (prefix, stat.S_IMODE(st.st_mode)))
    if st.st_ino != t["inode"]:
        problems.append("%s: inode %d != expected %d" % (prefix, st.st_ino, t["inode"]))
    if st.st_nlink != t["nlink"]:
        problems.append("%s: nlink %d != expected %d" % (prefix, st.st_nlink, t["nlink"]))
    if sha256(t["path"]) != t["orig_sha"]:
        problems.append("%s: sha mismatch vs pinned original" % prefix)
    return dict(inode=st.st_ino, nlink=st.st_nlink, mode=oct(stat.S_IMODE(st.st_mode)),
                uid=st.st_uid, gid=st.st_gid, size=st.st_size, dev=st.st_dev)


def preflight(run_dir):
    receipt = dict(when=time.strftime("%Y-%m-%dT%H:%M:%S%z"), nonce=uuid.uuid4().hex[:12],
                   candidate=CANDIDATE, candidate_sha=CANDIDATE_SHA, targets={}, problems=[],
                   wrapper_sha=sha256(os.path.abspath(__file__)))
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
    receipt["workers"], werrs = snapshot_workers()
    receipt["worker_count"] = len(receipt["workers"])
    receipt["worker_read_errors"] = werrs
    for e in werrs:
        p.append("worker /proc read error: %s" % e)
    receipt["hooks"] = snapshot_hooks()
    st = receipt["targets"].get("T3b") or {}
    links_key = "shared_inode_%d_links" % T3_INO
    if st.get("dev") is not None and st.get("inode") == T3_INO:
        links, partial = sweep_samefile(st["dev"], T3_INO, time.monotonic() + SWEEP_SECONDS)
        receipt[links_key] = dict(paths=links, sweep_partial=partial,
                                  named=[t["path"] for t in TARGETS[3:]])
        for lp in links:
            if lp not in [t["path"] for t in TARGETS[3:]]:
                l = lstat_or_none(lp)
                if l is None or sha256(lp) != T3_SHA:
                    p.append("unnamed link already divergent: %s" % lp)
    os.makedirs(run_dir, exist_ok=True)
    durable_write_json(run_dir, "receipt.json", receipt)
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
    try:
        src_fd = os.open(src, os.O_RDONLY | os.O_NOFOLLOW)
    except BaseException:
        os.close(dst_fd)
        raise
    try:
        with os.fdopen(src_fd, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                os.write(dst_fd, chunk)
                h.update(chunk)
    finally:
        os.fsync(dst_fd)
        os.close(dst_fd)
    st = os.lstat(dst_path)
    if h.hexdigest() != want_sha:
        raise RuntimeError("copy sha mismatch %s" % dst_path)
    if not stat.S_ISREG(st.st_mode) or stat.S_IMODE(st.st_mode) != want_mode \
            or st.st_uid != want_uid:
        raise RuntimeError("copy meta mismatch %s" % dst_path)


def make_file_same_dir(directory, basename, suffix, nonce, mode):
    """O_EXCL|O_NOFOLLOW regular owned file in the target's own directory."""
    path = os.path.join(directory, ".%s%s-%s" % (basename, suffix, nonce))
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    try:
        os.fchmod(fd, mode)  # a umask can mask os.open's mode argument
        fst = os.fstat(fd)
        if not stat.S_ISREG(fst.st_mode) or fst.st_uid != os.geteuid():
            raise RuntimeError("created file not regular/owned: %s" % path)
    except BaseException:
        os.close(fd)
        try:
            os.unlink(path)
        except OSError:
            pass
        raise
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
    manifest = dict(nonce=nonce, run_dir=run_dir, started=receipt["when"], script_rev=3,
                    candidate_sha=CANDIDATE_SHA, wrapper_sha=receipt["wrapper_sha"],
                    workers_before=receipt["workers"], hooks_before=receipt["hooks"],
                    committed=[], done=False, status="in-progress")
    durable_write_json(run_dir, "manifest.json", manifest)
    committed = []
    try:
        for t in TARGETS:
            directory = os.path.dirname(t["path"])
            base = os.path.basename(t["path"])
            st = lstat_or_none(t["path"])
            if st is None or not stat.S_ISREG(st.st_mode) or stat.S_IMODE(st.st_mode) != ORIG_MODE \
                    or st.st_uid != EXPECTED_UID or sha256(t["path"]) != t["orig_sha"]:
                raise RuntimeError("pre-rename drift on %s" % t["name"])
            bfd, bpath = make_file_same_dir(directory, base, ".orig", nonce, ORIG_MODE)
            copy_verified(t["path"], bfd, bpath, t["orig_sha"], ORIG_MODE, EXPECTED_UID)
            bst = os.lstat(bpath)
            tfd, tpath = make_file_same_dir(directory, base, ".new", nonce, NEW_MODE)
            copy_verified(CANDIDATE, tfd, tpath, CANDIDATE_SHA, NEW_MODE, EXPECTED_UID)
            tst = os.lstat(tpath)
            entry = dict(name=t["name"], path=t["path"], orig_sha=t["orig_sha"],
                         backup=dict(path=bpath, dev=bst.st_dev, ino=bst.st_ino,
                                     uid=bst.st_uid, gid=bst.st_gid,
                                     mode=format(ORIG_MODE, "o"), sha=t["orig_sha"]),
                         install=dict(dev=tst.st_dev, ino=tst.st_ino, uid=tst.st_uid,
                                      gid=tst.st_gid, mode=format(NEW_MODE, "o"),
                                      sha=CANDIDATE_SHA),
                         state="intended")
            committed.append(entry)
            manifest["committed"] = committed
            # Durable intent BEFORE the target rename: the journal now holds
            # backup custody and the exact install identity for this target,
            # so a crash after the rename is always recoverable.
            durable_write_json(run_dir, "manifest.json", manifest)
            os.rename(tpath, t["path"])
            fsync_dir(directory)
            st2 = lstat_or_none(t["path"])
            if st2 is None or not stat.S_ISREG(st2.st_mode) \
                    or stat.S_IMODE(st2.st_mode) != NEW_MODE or st2.st_uid != EXPECTED_UID \
                    or st2.st_dev != tst.st_dev or st2.st_ino != tst.st_ino \
                    or sha256(t["path"]) != CANDIDATE_SHA:
                raise RuntimeError("post-rename verify failed on %s" % t["name"])
            entry["state"] = "installed"
            durable_write_json(run_dir, "manifest.json", manifest)
            for other in TARGETS[3:]:
                if other["name"] == t["name"] or any(c["name"] == other["name"] for c in committed):
                    continue
                ost = lstat_or_none(other["path"])
                if ost is None or ost.st_ino != T3_INO or sha256(other["path"]) != T3_SHA:
                    raise RuntimeError("shared-inode sibling disturbed: %s" % other["name"])
        manifest["status"] = "applied"
        durable_write_json(run_dir, "manifest.json", manifest)
        problems = post_verify(receipt, manifest)
        if problems:
            manifest["post_verify_problems"] = problems
            raise RuntimeError("post-verify: %s" % "; ".join(problems))
    except BaseException as exc:
        manifest["done"] = True
        manifest["status"] = "interrupted" if isinstance(exc, KeyboardInterrupt) else "failed"
        manifest["failure"] = "%s: %s" % (type(exc).__name__, exc)
        try:
            rolled, skipped = rollback_committed(manifest)
            manifest["rollback"] = dict(rolled=rolled, skipped=skipped)
        except BaseException as rexc:
            manifest["rollback_error"] = "%s: %s" % (type(rexc).__name__, rexc)
        try:
            durable_write_json(run_dir, "manifest.json", manifest)
        except OSError as wexc:
            print("WARNING: journal write failed: %s" % wexc)
        print("STOPPED at first discrepancy: %s" % exc)
        print("rolled back: %s; skipped (concurrent custody): %s"
              % (manifest.get("rollback", {}).get("rolled", []),
                 manifest.get("rollback", {}).get("skipped", [])))
        print("receipt: %s" % run_dir)
        sys.exit(1)
    manifest["status"] = "success"
    manifest["done"] = True
    durable_write_json(run_dir, "manifest.json", manifest)
    print("APPLIED nonce=%s receipt=%s" % (nonce, run_dir))


def post_verify(before, manifest):
    problems = []
    links_key = "shared_inode_%d_links" % T3_INO
    for t in TARGETS:
        st = lstat_or_none(t["path"])
        if st is None or not stat.S_ISREG(st.st_mode) or stat.S_IMODE(st.st_mode) != NEW_MODE \
                or st.st_uid != EXPECTED_UID or sha256(t["path"]) != CANDIDATE_SHA:
            problems.append("%s post-state wrong" % t["name"])
    for lp in before.get(links_key, {}).get("paths", []):
        if lp in [t["path"] for t in TARGETS[3:]]:
            continue
        st = lstat_or_none(lp)
        if st is None or st.st_ino != T3_INO or sha256(lp) != T3_SHA:
            problems.append("unnamed link changed: %s" % lp)
    now, errs = snapshot_workers()
    if now != manifest["workers_before"]:
        problems.append("worker birth/exe snapshot changed")
    for e in errs:
        problems.append("worker /proc read error post: %s" % e)
    if snapshot_hooks() != manifest["hooks_before"]:
        problems.append("hook config hashes changed")
    return problems


def entry_installed_matches(entry):
    """True only if the on-disk file is still exactly the file this run
    installed: recorded dev/inode/uid/mode AND candidate sha. A concurrent
    same-sha replacement creates a new inode and fails this check, so
    rollback never clobbers it."""
    inst = entry.get("install") or {}
    st = lstat_or_none(entry["path"])
    if st is None or not stat.S_ISREG(st.st_mode):
        return False
    if (st.st_dev, st.st_ino, st.st_uid) != (inst.get("dev"), inst.get("ino"), inst.get("uid")):
        return False
    try:
        want_mode = int(inst.get("mode", ""), 8)
    except ValueError:
        return False
    if stat.S_IMODE(st.st_mode) != want_mode:
        return False
    return sha256(entry["path"]) == CANDIDATE_SHA


def backup_pin_ok(entry):
    bp = entry.get("backup") or {}
    bpath = bp.get("path")
    if not bpath:
        return False
    bst = lstat_or_none(bpath)
    if bst is None or not stat.S_ISREG(bst.st_mode):
        return False
    try:
        want_mode = int(bp.get("mode", ""), 8)
    except ValueError:
        return False
    if (bst.st_dev, bst.st_ino, bst.st_uid, bst.st_gid) != \
            (bp.get("dev"), bp.get("ino"), bp.get("uid"), bp.get("gid")):
        return False
    return stat.S_IMODE(bst.st_mode) == want_mode and sha256(bpath) == entry.get("orig_sha")


def rollback_committed(manifest):
    rolled, skipped = [], []
    for entry in reversed(manifest.get("committed", [])):
        name = entry.get("name", "?")
        if not entry_installed_matches(entry):
            skipped.append(dict(name=name, path=entry["path"],
                                reason="installed identity or sha changed concurrently"))
            continue
        if not backup_pin_ok(entry):
            skipped.append(dict(name=name, path=entry["path"],
                                reason="backup pin unverified"))
            continue
        bpath = entry["backup"]["path"]
        directory = os.path.dirname(entry["path"])
        os.rename(bpath, entry["path"])
        fsync_dir(directory)
        st3 = lstat_or_none(entry["path"])
        if st3 is None or not stat.S_ISREG(st3.st_mode) or stat.S_IMODE(st3.st_mode) != ORIG_MODE \
                or st3.st_uid != EXPECTED_UID or sha256(entry["path"]) != entry["orig_sha"]:
            raise RuntimeError("rollback verify failed %s" % entry["path"])
        entry["state"] = "rolled-back"
        rolled.append(entry["path"])
    return rolled, skipped


def rollback_cmd(manifest_path, force):
    with open(manifest_path) as f:
        manifest = json.load(f)
    status = manifest.get("status")
    if status not in FINALIZED and not force:
        sys.exit("refusing: run %s status %r is not finalized; pass --force to recover an "
                 "interrupted transaction (installed-identity checks still apply)"
                 % (manifest.get("nonce"), status))
    if not manifest.get("committed"):
        sys.exit("refusing: no committed targets recorded in %s" % manifest_path)
    rolled, skipped = rollback_committed(manifest)
    manifest["rollback"] = dict(rolled=rolled, skipped=skipped)
    manifest["done"] = True
    manifest["status"] = "rolled-back" if not skipped else "rolled-back-partial"
    durable_write_json(manifest.get("run_dir") or os.path.dirname(os.path.abspath(manifest_path)),
                       "manifest.json", manifest)
    print("rolled back: %s; skipped: %s" % (rolled, skipped))


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("command", choices=["preflight", "apply", "rollback"])
    ap.add_argument("--manifest", help="manifest.json of a run (rollback)")
    ap.add_argument("--force", action="store_true",
                    help="rollback: recover a not-finalized (interrupted) run journal")
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
        rollback_cmd(a.manifest, a.force)


if __name__ == "__main__":
    main()
