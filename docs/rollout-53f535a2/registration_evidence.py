"""Read-only Linux registration evidence. UNKNOWN is never terminal proof.

No RPC, signal, record write, cgroup change, executable launch or state report.
Containment validation follows src/cgroup/identity.rs and recovery.rs; an
inherited worker_cgroup/workload_cgroup observation is not a containment handle.
"""
import hashlib
import json
import os
from pathlib import Path
import stat

STATE_ROOT = '/home/alexey/.local/state/aplexer/sessions'
CGROUP_ROOT = '/sys/fs/cgroup'


def read_owned_json(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode) or before.st_uid != os.geteuid():
            raise RuntimeError('source not regular/owned: ' + str(path))
        data = bytearray()
        while True:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            data.extend(chunk)
            if len(data) > 1 << 20:
                raise RuntimeError('oversized registration source')
        after = os.fstat(fd)
        if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != \
                (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
            raise RuntimeError('registration changed while reading')
        parsed = json.loads(data)
        if not isinstance(parsed, dict):
            raise RuntimeError('registration source must be a JSON object')
        return parsed, dict(path=str(path), sha256=hashlib.sha256(data).hexdigest(),
                                     dev=before.st_dev, ino=before.st_ino, uid=before.st_uid)
    finally:
        os.close(fd)


def observer_domain():
    try:
        boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
        status = Path('/proc/self/status').read_text()
        nspid = next(line.split()[1:] for line in status.splitlines() if line.startswith('NSpid:'))
        if len(nspid) != 1:
            raise RuntimeError('caller is not in initial PID namespace')
        # A root cgroup2 mount is required; a subtree/lookalike cannot prove absence.
        mounts = [line.split() for line in Path('/proc/self/mountinfo').read_text().splitlines()
                  if ' - cgroup2 ' in line and line.split()[4] == CGROUP_ROOT]
        if len(mounts) != 1 or mounts[0][3] != '/':
            raise RuntimeError('cgroup2 root mount not positively identified')
        root = os.stat(CGROUP_ROOT)
        if not stat.S_ISDIR(root.st_mode) or not stat.S_ISREG(os.stat(CGROUP_ROOT + '/cgroup.controllers').st_mode):
            raise RuntimeError('invalid cgroup2 root')
        cg, mnt = os.stat('/proc/self/ns/cgroup'), os.stat('/proc/self/ns/mnt')
        fd = os.open(CGROUP_ROOT, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            info = Path('/proc/self/fdinfo/%s' % fd).read_text()
            mount_id = int(next(line.split(':')[1].strip() for line in info.splitlines()
                                if line.startswith('mnt_id:')))
        finally:
            os.close(fd)
        identity = dict(boot_id=boot, cgroup_namespace_device=cg.st_dev,
                        cgroup_namespace_inode=cg.st_ino, mount_namespace_device=mnt.st_dev,
                        mount_namespace_inode=mnt.st_ino, cgroup_mount_id=mount_id,
                        cgroup_root_device=root.st_dev, cgroup_root_inode=root.st_ino)
        return dict(status='VERIFIED', initial_pid_namespace=True, identity=identity)
    except (OSError, ValueError, StopIteration, RuntimeError) as exc:
        return dict(status='UNKNOWN', reason='%s: %s' % (type(exc).__name__, exc))


def process_probe(pid):
    if pid is None:
        return dict(status='NOT_RECORDED')
    if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0:
        return dict(status='UNKNOWN', reason='invalid recorded PID')
    path = Path('/proc') / str(pid)
    try:
        raw = (path / 'stat').read_text()
        fields = raw[raw.rindex(')') + 2:].split()
        observation = dict(status='PRESENT', pid=pid, state=fields[0], ppid=int(fields[1]),
                           process_group=int(fields[2]), session=int(fields[3]), birth=fields[19])
        # A zombie still exists. It is never substituted for absent/empty proof.
        return observation
    except FileNotFoundError as exc:
        # Missing stat while the process directory still exists is a measurement failure.
        try:
            os.stat(path)
        except FileNotFoundError:
            return dict(status='ABSENT', pid=pid, evidence='stat AND process directory ENOENT')
        except OSError as error:
            return dict(status='UNKNOWN', pid=pid, reason=str(error))
        return dict(status='UNKNOWN', pid=pid, reason='stat unavailable for existing process directory')
    except (OSError, ValueError, IndexError) as exc:
        return dict(status='UNKNOWN', pid=pid, reason='%s: %s' % (type(exc).__name__, exc))


def cgroup_observation(path):
    """Observation only: inherited or shared scopes confer no terminal proof."""
    try:
        leaf = os.lstat(path)
        if not stat.S_ISDIR(leaf.st_mode):
            raise RuntimeError('not nonsymlink directory')
        events = dict(line.split() for line in Path(path, 'cgroup.events').read_text().splitlines())
        if events.get('populated') not in ('0', '1'):
            raise RuntimeError('invalid populated measurement')
        pids = [int(pid) for pid in Path(path, 'cgroup.procs').read_text().split()]
        return dict(status='POPULATED' if events['populated'] == '1' else 'EMPTY',
                    path=path, dev=leaf.st_dev, ino=leaf.st_ino, pids=pids)
    except FileNotFoundError as exc:
        return dict(status='ABSENT_OBSERVED', path=path, reason=str(exc))
    except (OSError, ValueError, RuntimeError) as exc:
        return dict(status='UNKNOWN', path=path, reason='%s: %s' % (type(exc).__name__, exc))


def containment_probe(record, domain):
    locator = record.get('containment_cgroup')
    saved = record.get('containment_cgroup_identity')
    if not locator or not saved:
        return dict(status='UNKNOWN', reason='no authoritative recorded containment locator/identity')
    try:
        sid = str(__import__('uuid').UUID(record['id']))
        path = Path(locator)
        if domain.get('status') != 'VERIFIED' or saved != domain['identity']:
            raise RuntimeError('recorded containment boot/namespace/mount identity not verified')
        root = Path(CGROUP_ROOT)
        relative = path.relative_to(root)
        if not path.is_absolute() or '..' in relative.parts or \
                path.name != 'aplexer-workload-%s.scope' % sid:
            raise RuntimeError('locator not UUID-bound under cgroup root')
        # Walk through root-anchored descriptors; never follow symlinks or
        # confuse EACCES/ENOTDIR with a missing cgroup directory.
        fd = os.open(CGROUP_ROOT, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        absent = False
        try:
            for part in relative.parts:
                try:
                    nxt = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                except FileNotFoundError:
                    absent = True
                    break
                os.close(fd)
                fd = nxt
            if absent:
                result = dict(status='ABSENT_VERIFIED', path=locator,
                              evidence='UUID-bound scope directory ENOENT inside exact recorded cgroup2 domain')
            else:
                st = os.fstat(fd)
                if st.st_dev != saved['cgroup_root_device']:
                    raise RuntimeError('scope escaped recorded cgroup filesystem')
                def read_at(name):
                    child = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd)
                    try:
                        with os.fdopen(child, 'r') as f:
                            return f.read()
                    except BaseException:
                        # fdopen owns the descriptor once constructed.
                        raise
                events = dict(line.split() for line in read_at('cgroup.events').splitlines())
                members = [int(pid) for pid in read_at('cgroup.procs').split()]
                if events.get('populated') == '0' and not members:
                    result = dict(status='EMPTY_VERIFIED', path=locator, dev=st.st_dev, ino=st.st_ino)
                elif events.get('populated') == '1':
                    result = dict(status='POPULATED', path=locator, pids=members)
                else:
                    raise RuntimeError('invalid or contradictory cgroup measurement')
        finally:
            os.close(fd)
        if observer_domain() != domain:
            raise RuntimeError('observer domain changed during containment measurement')
        return result
    except (OSError, ValueError, KeyError, RuntimeError) as exc:
        return dict(status='UNKNOWN', path=locator, reason='%s: %s' % (type(exc).__name__, exc))


def classify_registration(record, identity, worker, workload, containment, domain):
    """Pure decision table. Numerical PID absence alone never becomes terminal."""
    result = dict(classification='UNKNOWN')
    if type(identity.get('pid')) is not int or identity['pid'] <= 0 or \
            type(identity.get('start_time_ticks')) is not int or identity['start_time_ticks'] <= 0:
        result['reason'] = 'invalid recorded native PID/birth identity'
    elif domain.get('status') != 'VERIFIED' or not domain.get('initial_pid_namespace'):
        result['reason'] = 'observer absence authority unverified'
    elif identity.get('pid') != record.get('worker_pid') or \
            identity.get('boot_id') != domain['identity'].get('boot_id'):
        result['reason'] = 'registration/boot identity mismatch'
    elif worker.get('status') == 'PRESENT':
        if worker.get('birth') != str(identity.get('start_time_ticks')):
            result['reason'] = 'recorded numeric worker PID reused; present actor must be preserved'
        elif worker.get('state') in ('Z', 'X', 'x'):
            result['reason'] = 'worker corpse still present; absence not proved'
        elif workload.get('status') == 'UNKNOWN':
            result['reason'] = 'live worker, workload measurement unknown'
        else:
            result = dict(classification='LIVE_WORKER', reason='exact registered PID/boot/birth exists')
    elif worker.get('status') != 'ABSENT':
        result['reason'] = 'worker measurement unknown'
    elif workload.get('status') == 'PRESENT':
        result = dict(classification='LIVE_WORKLOAD', reason='worker absent but recorded workload PID exists; birth not durably pinned')
    elif workload.get('status') not in ('ABSENT', 'NOT_RECORDED'):
        result['reason'] = 'workload absence unknown'
    elif containment.get('status') in ('ABSENT_VERIFIED', 'EMPTY_VERIFIED'):
        result = dict(classification='TERMINAL_ABSENT',
                      reason='registered worker/workload absent and authoritative UUID-bound containment empty in verified domain')
    else:
        result['reason'] = 'worker/workload absence does not prove descendants terminal; containment ' + containment.get('status', 'UNKNOWN')
    return result


def inspect_registration(record, identity, domain):
    worker = process_probe(record.get('worker_pid'))
    workload = process_probe(record.get('workload_pid'))
    containment = containment_probe(record, domain)
    # A second scoped measurement closes the obvious PID reuse/disappearance gap.
    again_worker, again_workload = process_probe(record.get('worker_pid')), process_probe(record.get('workload_pid'))
    def identity_signature(probe):
        return (probe.get('status'), probe.get('pid'), probe.get('birth'),
                probe.get('state') in ('Z', 'X', 'x'))
    if identity_signature(worker) != identity_signature(again_worker) or \
            identity_signature(workload) != identity_signature(again_workload) or observer_domain() != domain:
        decision = dict(classification='UNKNOWN', reason='identity/observer changed during inspection')
    else:
        decision = classify_registration(record, identity, worker, workload, containment, domain)
    return dict(**decision, worker=worker, workload=workload, containment=containment,
                observer=domain, phase=record.get('phase'), uuid=record.get('id'),
                recorded_identity=identity, worker_cgroup=record.get('worker_cgroup'),
                workload_cgroup=record.get('workload_cgroup'), containment_empty=record.get('containment_empty'))


def registration_inventory(state_root=STATE_ROOT):
    protected, classifications, errors = {}, {}, []
    domain = observer_domain()
    for path in sorted(Path(state_root).glob('*/session.json')):
        try:
            record, source = read_owned_json(path)
            identity, identity_source = read_owned_json(path.with_name('worker.identity.json'))
            # Prior scope was running registrations; also protect exact live
            # registered workers even if a persisted terminal phase contradicts them.
            worker = process_probe(record.get('worker_pid'))
            workload = process_probe(record.get('workload_pid'))
            if str(path.parent.name) != record.get('id') or identity.get('pid') != record.get('worker_pid'):
                raise RuntimeError('directory/registration/identity binding mismatch')
            if type(identity.get('pid')) is not int or identity['pid'] <= 0 or \
                    type(identity.get('start_time_ticks')) is not int or identity['start_time_ticks'] <= 0:
                raise RuntimeError('invalid recorded native PID/birth identity')
            if record.get('phase') not in ('running', 'starting', 'exiting') and \
                    worker.get('status') == 'ABSENT' and workload.get('status') in ('ABSENT', 'NOT_RECORDED') and \
                    domain.get('status') == 'VERIFIED' and identity.get('boot_id') == domain['identity']['boot_id']:
                classifications[str(path.parent.name)] = dict(classification='OUTSIDE_PRIOR_ACTIVE_SCOPE',
                    reason='inactive registration; no claim of terminal containment', source=source,
                    recorded_identity=identity, worker=worker, workload=workload, phase=record.get('phase'))
                continue
            item = inspect_registration(record, identity, domain)
            item.update(source=source, identity_source=identity_source)
            classifications[record['id']] = item
            if item['classification'] == 'LIVE_WORKER':
                pid = str(identity['pid'])
                if pid in protected:
                    raise RuntimeError('duplicate registered native PID')
                protected[pid] = dict(birth=str(identity['start_time_ticks']), session=record['id'])
            elif item['classification'] != 'TERMINAL_ABSENT':
                errors.append('registration %s: %s: %s' % (record['id'], item['classification'], item['reason']))
        except (OSError, ValueError, KeyError, RuntimeError) as exc:
            classifications[str(path.parent.name)] = dict(classification='UNKNOWN', source=str(path), reason=str(exc))
            errors.append('registration %s: %s' % (path, exc))
    return protected, classifications, errors
