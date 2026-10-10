"""Transaction fault controls. All mutations stay in fresh temporary fixtures."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('rollout', Path(__file__).with_name('apply_rollout.py'))
r = importlib.util.module_from_spec(spec)
spec.loader.exec_module(r)


class Transactions(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.receipts = self.root / 'receipts'
        self.receipts.mkdir()
        self.candidate = self.root / 'candidate'
        self.candidate.write_bytes(b'NEW ELF fixture\n' * 1000)
        self.original = b'ORIGINAL ELF fixture\n' * 1000
        self.targets = []
        for i in range(6):
            path = self.root / ('target%d' % i)
            if i >= 4:
                os.link(self.targets[3]['path'], path)
            else:
                path.write_bytes(self.original)
                path.chmod(0o775)
            st = path.stat()
            self.targets.append(dict(name='T%d' % i, path=str(path), inode=st.st_ino,
                                     orig_sha=r.sha256(str(path)), nlink=5 if i >= 3 else 1))
        self.unnamed = [self.root / 'unnamed1', self.root / 'unnamed2']
        for p in self.unnamed:
            os.link(self.targets[3]['path'], p)
        self.patchers = [patch.object(r, k, v) for k, v in dict(
            EXPECTED_UID=os.geteuid(), CANDIDATE=str(self.candidate),
            CANDIDATE_SHA=r.sha256(str(self.candidate)), TARGETS=self.targets,
            T3_SHA=self.targets[3]['orig_sha'], T3_INO=self.targets[3]['inode'],
            RECEIPT_ROOT=str(self.receipts), SWEEP_ROOTS=[str(self.root)]).items()]
        # Existing host contract uses uid == gid == 1000.
        self.assertEqual(os.geteuid(), os.getegid())
        self.patchers += [patch.object(r, 'snapshot_workers', return_value=({}, [])),
                          patch.object(r, 'snapshot_hooks', return_value={})]
        for p in self.patchers:
            p.start()
        self.addCleanup(self.tmp.cleanup)
        for p in self.patchers:
            self.addCleanup(p.stop)

    def apply(self, fails=False):
        with contextlib.redirect_stdout(io.StringIO()):
            if fails:
                with self.assertRaises(SystemExit) as exc:
                    r.apply()
                self.assertEqual(exc.exception.code, 1)
            else:
                r.apply()
        paths = list(self.receipts.glob('run-*/manifest.json'))
        self.assertEqual(len(paths), 1)
        return json.loads(paths[0].read_text())

    def assert_original(self):
        for target in self.targets:
            self.assertEqual(Path(target['path']).read_bytes(), self.original)
            self.assertEqual(Path(target['path']).stat().st_mode & 0o777, 0o775)

    def test_six_fresh_inodes_preserve_unnamed_hardlinks_and_restore_modes(self):
        old_umask = os.umask(0o077)
        try:
            manifest = self.apply()
        finally:
            os.umask(old_umask)
        self.assertEqual(manifest['status'], 'success')
        for entry in manifest['committed']:
            self.assertTrue(r.entry_installed_matches(entry))
            self.assertTrue(r.backup_pin_ok(entry))
            self.assertNotEqual(entry['install']['ino'], entry['original']['ino'])
        for p in self.unnamed:
            self.assertEqual(p.stat().st_ino, r.T3_INO)
            self.assertEqual(p.read_bytes(), self.original)
        rolled, skipped = r.rollback_committed(manifest)
        self.assertEqual(len(rolled), 6)
        self.assertEqual(skipped, [])
        self.assert_original()

    def test_copy_loops_shortwrites_and_hashes_actual_destination(self):
        real_write = os.write
        def short(fd, data):
            return real_write(fd, data[:17])
        with patch.object(r.os, 'write', short):
            self.apply()
        for t in self.targets:
            self.assertEqual(Path(t['path']).read_bytes(), self.candidate.read_bytes())

    def test_copy_corruption_and_zero_progress_close_owned_descriptor(self):
        for action in ['zero', 'corrupt']:
            fd, path = r.make_file_same_dir(str(self.root), 'test', '.new', action, 0o755)
            write = os.write
            def bad(fd, data):
                return 0 if action == 'zero' else write(fd, bytes(len(data)))
            with patch.object(r.os, 'write', bad):
                with self.assertRaises((OSError, RuntimeError)):
                    r.copy_verified(str(self.candidate), fd, path, r.CANDIDATE_SHA, 0o755, os.geteuid())
            with self.assertRaises(OSError):
                os.fstat(fd)

    def test_postverify_failure_rolls_back_every_owned_entry(self):
        with patch.object(r, 'post_verify', side_effect=OSError('protected worker read failed')):
            manifest = self.apply(fails=True)
        self.assertEqual(len(manifest['rollback']['rolled']), 6)
        self.assert_original()

    def test_immediate_target_drift_after_copy_is_not_overwritten(self):
        real = r.copy_verified
        new_identity = []
        def copy(*args):
            real(*args)
            if args[0] == str(self.candidate) and not new_identity:
                target = self.targets[0]['path']
                other = self.root / 'concurrent'
                other.write_bytes(self.original)
                other.chmod(0o775)
                os.replace(other, target)
                new_identity.append(os.stat(target).st_ino)
        with patch.object(r, 'copy_verified', copy):
            manifest = self.apply(fails=True)
        self.assertEqual(os.stat(self.targets[0]['path']).st_ino, new_identity[0])
        self.assertEqual(manifest['committed'][0]['state'], 'intended')
        self.assert_original()

    def test_rename_then_fsync_failure_remains_in_durable_intent_and_rolls_back(self):
        real = r.checked_rename
        once = []
        def rename(*args):
            journal = json.loads(next(self.receipts.glob('run-*/manifest.json')).read_text())
            if not once:
                self.assertEqual(journal['committed'][-1]['state'], 'intended')
                self.assertTrue(r.backup_pin_ok(journal['committed'][-1]))
                self.assertEqual(r.file_pin(args[0]), journal['committed'][-1]['install'])
            real(*args)
            if not once:
                once.append(True)
                raise OSError('after rename directory fsync failure')
        with patch.object(r, 'checked_rename', rename):
            manifest = self.apply(fails=True)
        self.assertEqual(len(manifest['rollback']['rolled']), 1)
        self.assert_original()

    def test_same_hash_concurrent_replacement_and_bad_backup_are_skipped(self):
        manifest = self.apply()
        entry = manifest['committed'][-1]
        other = self.root / 'concurrent'
        other.write_bytes(self.candidate.read_bytes())
        other.chmod(0o755)
        os.replace(other, entry['path'])
        replaced = os.stat(entry['path']).st_ino
        bad = manifest['committed'][-2]
        os.unlink(bad['backup']['path'])
        os.symlink(str(self.candidate), bad['backup']['path'])
        rolled, skipped = r.rollback_committed(manifest)
        self.assertEqual(len(rolled), 4)
        self.assertEqual(len(skipped), 2)
        self.assertEqual(os.stat(entry['path']).st_ino, replaced)

    def test_one_rollback_exception_does_not_abandon_other_entries(self):
        manifest = self.apply()
        real = r.checked_rename
        def rename(src, dst, parent):
            if dst == manifest['committed'][-1]['path']:
                raise OSError('one owned restore failed')
            real(src, dst, parent)
        with patch.object(r, 'checked_rename', rename):
            rolled, skipped = r.rollback_committed(manifest)
        self.assertEqual(len(rolled), 5)
        self.assertEqual(len(skipped), 1)

    def test_journal_failed_write_preserves_previous_atomic_publication(self):
        run = self.receipts / 'atomic'
        run.mkdir()
        r.durable_write_json(str(run), 'manifest.json', {'state': 'intended'})
        with patch.object(r.os, 'write', side_effect=OSError('disk failure')):
            with self.assertRaises(OSError):
                r.durable_write_json(str(run), 'manifest.json', {'state': 'committed'})
        self.assertEqual(json.loads((run / 'manifest.json').read_text()), {'state': 'intended'})
        self.assertEqual(list(run.glob('.journal*')), [])

    def test_exclusive_temp_does_not_overwrite_existing_file(self):
        fd, path = r.make_file_same_dir(str(self.root), 'same', '.new', 'nonce', 0o755)
        os.close(fd)
        with self.assertRaises(FileExistsError):
            r.make_file_same_dir(str(self.root), 'same', '.new', 'nonce', 0o755)
        self.assertTrue(Path(path).is_file())

    def test_worker_image_identity_ignores_deleted_suffix_without_process_census(self):
        # Actual held native image metadata; textual suffix is a fixture.
        pid = str(os.getpid())
        raw = Path('/proc/' + pid + '/stat').read_text()
        protected = {pid: dict(birth=raw[raw.rindex(')') + 2:].split()[19], session='fixture')}
        # Bypass only this suite's no-host worker mock for this read-only control.
        implementation = spec.loader.get_source('rollout')
        namespace = {'__file__': r.__file__}
        exec(compile(implementation, r.__file__, 'exec'), namespace)
        snapshot = namespace['snapshot_workers']
        before, errors = snapshot(protected)
        self.assertEqual(errors, [])
        real_link = os.readlink
        with patch.object(os, 'readlink', lambda path: real_link(path) + ' (deleted)'), \
                patch.object(os, 'listdir', side_effect=AssertionError('global census forbidden')):
            after, errors = snapshot(protected)
        self.assertEqual(errors, [])
        self.assertEqual(before, after)


if __name__ == '__main__':
    unittest.main()
