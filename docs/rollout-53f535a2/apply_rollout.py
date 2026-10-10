#!/usr/bin/env python3
"""Administrative rollout wrapper: candidate 53f535a2 -> six installed aplexer ELFs.

Frozen administrative fix, rev 5. Not a product code path: this file only
orchestrates a verified, atomic, per-target replace of the six named ELF
directory entries. It never touches Python shims, hook configs, or the
running supervisor/workers, and it never opens a target for writing --
replacement is rename(2) over the directory entry only, so hardlink
siblings of a replaced inode keep the old inode and content untouched.

Rev 5 preserves refused d5c8119 and 7a06f29 receipts. Preflight reads metadata and full hashes
only; it never runs the candidate or invokes the product state path.
Protected workers are registered PID/birth/image-inode identities. Existing
workers retain existing freshness behavior. This wrapper cannot restart them.

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
import sys
import time
import uuid
import registration_evidence

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


def hash_fd(fd):
    os.lseek(fd, 0, os.SEEK_SET)
    h = hashlib.sha256()
    for chunk in iter(lambda: os.read(fd, 1 << 20), b""):
        h.update(chunk)
    return h.hexdigest()


def file_pin(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode):
            raise RuntimeError("not regular: %s" % path)
        digest = hash_fd(fd)
        after = os.fstat(fd)
        current = os.lstat(path)
        def signature(value):
            return (value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
                    value.st_size, value.st_mtime_ns, value.st_ctime_ns)
        if signature(st) != signature(after) or signature(st) != signature(current):
            raise RuntimeError("file changed during verification: %s" % path)
        return dict(dev=st.st_dev, ino=st.st_ino, type=stat.S_IFMT(st.st_mode),
                    uid=st.st_uid, gid=st.st_gid, mode=format(stat.S_IMODE(st.st_mode), "o"),
                    size=st.st_size, sha=digest)
    finally:
        os.close(fd)


def sha256(path):
    return file_pin(path)["sha"]


def parent_pin(path):
    st = os.lstat(path)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != EXPECTED_UID:
        raise RuntimeError("parent not nonsymlink owned directory: %s" % path)
    return dict(dev=st.st_dev, ino=st.st_ino, uid=st.st_uid, gid=st.st_gid,
                type=stat.S_IFMT(st.st_mode), mode=stat.S_IMODE(st.st_mode))


def checked_rename(src, dst, parent):
    directory = os.path.dirname(dst)
    if os.path.dirname(src) != directory:
        raise RuntimeError("rename source must share target directory")
    if parent_pin(directory) != parent:
        raise RuntimeError("parent changed: %s" % directory)
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        st = os.fstat(fd)
        if (st.st_dev, st.st_ino) != (parent["dev"], parent["ino"]):
            raise RuntimeError("opened parent changed")
        os.rename(os.path.basename(src), os.path.basename(dst), src_dir_fd=fd, dst_dir_fd=fd)
        os.fsync(fd)
    finally:
        os.close(fd)


def lstat_or_none(path):
    try:
        return os.lstat(path)
    except OSError:
        return None


def open_parent(path, expected=None):
    pin = parent_pin(path)
    if expected is not None and pin != expected:
        raise RuntimeError("parent drift: %s" % path)
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    st = os.fstat(fd)
    if (st.st_dev, st.st_ino, st.st_uid, st.st_gid, stat.S_IMODE(st.st_mode)) != \
            (pin["dev"], pin["ino"], pin["uid"], pin["gid"], pin["mode"]):
        os.close(fd)
        raise RuntimeError("opened parent drift")
    return fd


def fsync_dir(path):
    fd = open_parent(path)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def durable_write_json(run_dir, final_name, payload):
    """Publish fully written, fsynced owned bytes atomically; fsync directory."""
    if os.path.basename(final_name) != final_name:
        raise RuntimeError("journal name must be a basename")
    tmp = ".journal.%s.%s.tmp" % (final_name, uuid.uuid4().hex)
    dfd = open_parent(run_dir)
    fd = None
    try:
        fd = os.open(tmp, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644, dir_fd=dfd)
        os.fchmod(fd, 0o644)
        data = json.dumps(payload, indent=1, sort_keys=True).encode()
        write_all(fd, data)
        os.fsync(fd)
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != EXPECTED_UID or \
                st.st_gid != EXPECTED_UID or stat.S_IMODE(st.st_mode) != 0o644 or \
                st.st_size != len(data) or hash_fd(fd) != hashlib.sha256(data).hexdigest():
            raise RuntimeError("journal descriptor verification failed")
        named = os.stat(tmp, dir_fd=dfd, follow_symlinks=False)
        if (named.st_dev, named.st_ino, named.st_mode, named.st_uid, named.st_gid) != \
                (st.st_dev, st.st_ino, st.st_mode, st.st_uid, st.st_gid):
            raise RuntimeError("journal temporary entry changed")
        os.rename(tmp, final_name, src_dir_fd=dfd, dst_dir_fd=dfd)
        os.fsync(dfd)
    except BaseException:
        try:
            os.unlink(tmp, dir_fd=dfd)
        except OSError:
            pass
        raise
    finally:
        if fd is not None:
            os.close(fd)
        os.close(dfd)


def snapshot_workers(protected=None, classifications=None):
    """Registered native workers only; held image identity, never a process census.

    Postcheck follows the exact preflight set, so unrelated CLI exits/starts
    cannot manufacture a restart. Missing protected workers fail closed.
    """
    out, errors = {}, []
    if protected is None:
        protected, inventory, errors = registration_evidence.registration_inventory()
        if classifications is not None:
            classifications.update(inventory)
    for pid, registered in protected.items():
        try:
            proc = "/proc/%s" % pid
            with open(proc + "/stat") as f:
                raw = f.read()
            birth = raw[raw.rindex(")") + 2:].split()[19]
            if birth != registered["birth"]:
                raise RuntimeError("registered worker birth mismatch")
            image = os.stat(proc + "/exe")
            exe = os.readlink(proc + "/exe").removesuffix(" (deleted)")
            map_dev = "%02x:%02x" % (os.major(image.st_dev), os.minor(image.st_dev))
            with open(proc + "/maps") as f:
                mappings = sorted(set(tuple(line.split()[3:5]) for line in f
                                      if len(line.split()) >= 5 and
                                      line.split()[4] == str(image.st_ino) and line.split()[3] == map_dev))
            with open(proc + "/stat") as f:
                final_stat = f.read()
            if final_stat[final_stat.rindex(")") + 2:].split()[19] != birth:
                raise RuntimeError("worker birth changed during observation")
            if not mappings:
                raise RuntimeError("executable inode not mapped")
            out[str(pid)] = dict(birth=birth, session=registered["session"], exe=exe,
                                 dev=image.st_dev, ino=image.st_ino, mappings=mappings)
        except (OSError, ValueError, IndexError, RuntimeError) as exc:
            errors.append("protected pid %s: %s" % (pid, exc))
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
            if time.monotonic() > deadline:
                return found, True
            for f in filenames:
                if time.monotonic() > deadline:
                    return found, True
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
    if st.st_gid != EXPECTED_UID:
        problems.append("%s: gid mismatch" % prefix)
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
    return dict(pin=file_pin(t["path"]), parent=parent_pin(os.path.dirname(t["path"])), inode=st.st_ino, nlink=st.st_nlink, mode=oct(stat.S_IMODE(st.st_mode)),
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
    for t in TARGETS:
        receipt["targets"][t["name"]] = check_target(t, p, t["name"]) or {}
    receipt["registration_evidence_sha"] = sha256(registration_evidence.__file__)
    receipt["worker_registration_classifications"] = {}
    receipt["workers"], werrs = snapshot_workers(classifications=receipt["worker_registration_classifications"])
    receipt["worker_count"] = len(receipt["workers"])
    receipt["worker_evidence_problems"] = werrs
    for e in werrs:
        p.append("registered worker evidence: %s" % e)
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
    os.mkdir(run_dir, 0o755)
    fsync_dir(os.path.dirname(run_dir))
    durable_write_json(run_dir, "receipt.json", receipt)
    return receipt


def write_all(fd, data):
    view = memoryview(data)
    while view:
        count = os.write(fd, view)
        if count <= 0:
            raise OSError("write made no progress")
        view = view[count:]


def copy_verified(src, dst_fd, dst_path, want_sha, want_mode, want_uid):
    src_fd = None
    try:
        os.fchmod(dst_fd, want_mode)
        src_fd = os.open(src, os.O_RDONLY | os.O_NOFOLLOW)
        source = os.fstat(src_fd)
        if not stat.S_ISREG(source.st_mode) or source.st_uid != want_uid:
            raise RuntimeError("source not regular/owned")
        for chunk in iter(lambda: os.read(src_fd, 1 << 20), b""):
            write_all(dst_fd, chunk)
        os.fsync(dst_fd)
        dest = os.fstat(dst_fd)
        if not stat.S_ISREG(dest.st_mode) or dest.st_uid != want_uid or dest.st_gid != EXPECTED_UID or \
                stat.S_IMODE(dest.st_mode) != want_mode or dest.st_size != source.st_size or \
                hash_fd(dst_fd) != want_sha:
            raise RuntimeError("destination bytes/metadata mismatch: %s" % dst_path)
        pin = file_pin(dst_path)
        if (pin["dev"], pin["ino"]) != (dest.st_dev, dest.st_ino) or pin["sha"] != want_sha:
            raise RuntimeError("destination directory entry changed")
    finally:
        if src_fd is not None:
            os.close(src_fd)
        os.close(dst_fd)


def make_file_same_dir(directory, basename, suffix, nonce, mode, parent=None):
    """Exclusive new inode through a verified owned directory descriptor."""
    name = ".%s%s-%s" % (basename, suffix, nonce)
    path = os.path.join(directory, name)
    dfd = open_parent(directory, parent)
    fd = None
    try:
        fd = os.open(name, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode, dir_fd=dfd)
        os.fchmod(fd, mode)
        fst = os.fstat(fd)
        if not stat.S_ISREG(fst.st_mode) or fst.st_uid != EXPECTED_UID or \
                fst.st_gid != EXPECTED_UID or stat.S_IMODE(fst.st_mode) != mode:
            raise RuntimeError("created file not regular/owned/exact mode: %s" % path)
        return fd, path
    except BaseException:
        if fd is not None:
            os.close(fd)
            os.unlink(name, dir_fd=dfd)
        raise
    finally:
        os.close(dfd)


def expected_links(t, receipt, committed):
    pin = receipt["targets"][t["name"]]["pin"]
    own_removals = sum(1 for entry in committed if entry["original"]["dev"] == pin["dev"] and
                       entry["original"]["ino"] == pin["ino"])
    return receipt["targets"][t["name"]]["nlink"] - own_removals


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
    manifest = dict(nonce=nonce, run_dir=run_dir, started=receipt["when"], script_rev=5,
                    candidate_sha=CANDIDATE_SHA, wrapper_sha=receipt["wrapper_sha"],
                    registration_evidence_sha=receipt["registration_evidence_sha"],
                    workers_before=receipt["workers"], hooks_before=receipt["hooks"],
                    committed=[], done=False, status="in-progress")
    durable_write_json(run_dir, "manifest.json", manifest)
    committed = []
    try:
        for t in TARGETS:
            directory = os.path.dirname(t["path"])
            base = os.path.basename(t["path"])
            parent = receipt["targets"][t["name"]]["parent"]
            original = file_pin(t["path"])
            if original != receipt["targets"][t["name"]]["pin"]:
                raise RuntimeError("target identity drift: %s" % t["name"])
            st = lstat_or_none(t["path"])
            if st is None or not stat.S_ISREG(st.st_mode) or stat.S_IMODE(st.st_mode) != ORIG_MODE \
                    or st.st_uid != EXPECTED_UID or sha256(t["path"]) != t["orig_sha"]:
                raise RuntimeError("pre-rename drift on %s" % t["name"])
            bfd, bpath = make_file_same_dir(directory, base, ".orig", nonce, ORIG_MODE, parent)
            copy_verified(t["path"], bfd, bpath, t["orig_sha"], ORIG_MODE, EXPECTED_UID)
            tfd, tpath = make_file_same_dir(directory, base, ".new", nonce, NEW_MODE, parent)
            copy_verified(CANDIDATE, tfd, tpath, CANDIDATE_SHA, NEW_MODE, EXPECTED_UID)
            fsync_dir(directory)  # backup and stage names durable before intent
            entry = dict(name=t["name"], path=t["path"], orig_sha=t["orig_sha"],
                         parent=parent, original=original,
                         backup=dict(path=bpath, **file_pin(bpath)),
                         install=file_pin(tpath), state="intended")
            link_count = expected_links(t, receipt, committed)
            committed.append(entry)
            manifest["committed"] = committed
            # Durable intent BEFORE the target rename: the journal now holds
            # backup custody and the exact install identity for this target,
            # so a crash after the rename is always recoverable.
            durable_write_json(run_dir, "manifest.json", manifest)
            # Last full target/parent/backup/stage check after copies and journal.
            if file_pin(tpath) != entry["install"] or not backup_pin_ok(entry) or \
                    parent_pin(directory) != parent or os.lstat(t["path"]).st_nlink != link_count or \
                    file_pin(t["path"]) != original:
                raise RuntimeError("immediate pre-rename drift: %s" % t["name"])
            checked_rename(tpath, t["path"], parent)
            entry["state"] = "committed"
            durable_write_json(run_dir, "manifest.json", manifest)
            if not entry_installed_matches(entry):
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
        manifest["status"] = "success"
        manifest["done"] = True
        durable_write_json(run_dir, "manifest.json", manifest)
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
    print("APPLIED nonce=%s receipt=%s" % (nonce, run_dir))


def post_verify(before, manifest):
    problems = []
    links_key = "shared_inode_%d_links" % T3_INO
    for entry in manifest["committed"]:
        if not entry_installed_matches(entry):
            problems.append("%s post-state wrong" % entry["name"])
    for lp in before.get(links_key, {}).get("paths", []):
        if lp in [t["path"] for t in TARGETS[3:]]:
            continue
        st = lstat_or_none(lp)
        if st is None or st.st_ino != T3_INO or sha256(lp) != T3_SHA:
            problems.append("unnamed link changed: %s" % lp)
    now, errs = snapshot_workers(manifest["workers_before"])
    if now != manifest["workers_before"]:
        problems.append("worker birth/exe snapshot changed")
    for e in errs:
        problems.append("worker /proc read error post: %s" % e)
    # A previously excluded terminal registration must still have positive
    # proof; an absent PID or a stale phase is never enough on its own.
    for sid, item in before.get("worker_registration_classifications", {}).items():
        if item.get("classification") != "TERMINAL_ABSENT":
            continue
        record, _ = registration_evidence.read_owned_json(item["source"]["path"])
        identity, _ = registration_evidence.read_owned_json(item["identity_source"]["path"])
        current = registration_evidence.inspect_registration(record, identity, registration_evidence.observer_domain())
        if current["classification"] != "TERMINAL_ABSENT" or current["uuid"] != sid or \
                identity != item["recorded_identity"]:
            problems.append("terminal proof changed for registration %s" % sid)
    if snapshot_hooks() != manifest["hooks_before"]:
        problems.append("hook config hashes changed")
    return problems


def entry_installed_matches(entry):
    return file_pin(entry["path"]) == entry["install"] and entry["install"]["sha"] == CANDIDATE_SHA


def backup_pin_ok(entry):
    bp = entry["backup"]
    pin = file_pin(bp["path"])
    return pin == {k: v for k, v in bp.items() if k != "path"} and \
        pin["sha"] == entry["orig_sha"] and pin["mode"] == format(ORIG_MODE, "o") and \
        pin["uid"] == EXPECTED_UID and pin["gid"] == EXPECTED_UID


def rollback_committed(manifest):
    rolled, skipped = [], []
    for entry in reversed(manifest.get("committed", [])):
        try:
            if not backup_pin_ok(entry) or not entry_installed_matches(entry):
                raise RuntimeError("installed identity or backup custody changed")
            directory = os.path.dirname(entry["path"])
            if parent_pin(directory) != entry["parent"]:
                raise RuntimeError("parent changed")
            entry["state"] = "rollback-intended"
            durable_write_json(manifest["run_dir"], "manifest.json", manifest)
            # Recheck after journal immediately before the scoped rename.
            if not backup_pin_ok(entry) or not entry_installed_matches(entry):
                raise RuntimeError("immediate rollback custody changed")
            checked_rename(entry["backup"]["path"], entry["path"], entry["parent"])
            restored = file_pin(entry["path"])
            if restored != {k: v for k, v in entry["backup"].items() if k != "path"}:
                raise RuntimeError("rollback post-verification failed")
            entry["state"] = "rolled-back"
            rolled.append(entry["path"])
            durable_write_json(manifest["run_dir"], "manifest.json", manifest)
        except Exception as exc:
            skipped.append(dict(name=entry.get("name", "?"), path=entry["path"],
                                reason="%s: %s" % (type(exc).__name__, exc)))
            # Each failure is isolated; continue reconciling all other entries.
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
    ap.add_argument("--run-dir", help="fresh preflight receipt directory (must not exist)")
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
