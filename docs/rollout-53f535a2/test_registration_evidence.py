"""Read-only registration decision/fault controls, separate from the preserved 11 transaction tests."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import uuid
import registration_evidence as e
import apply_rollout as rollout


class Evidence(unittest.TestCase):
    def setUp(self):
        self.sid = str(uuid.uuid4())
        self.domain = dict(status='VERIFIED', initial_pid_namespace=True,
                           identity=dict(boot_id='fixture-boot', cgroup_root_device=0))
        self.record = dict(id=self.sid, worker_pid=100, workload_pid=101, phase='running', containment_empty=False)
        self.identity = dict(pid=100, start_time_ticks=123, boot_id='fixture-boot')
        self.absent_worker = dict(status='ABSENT', pid=100)
        self.absent_workload = dict(status='ABSENT', pid=101)
        self.empty = dict(status='ABSENT_VERIFIED')

    def decide(self, **changes):
        values = dict(record=self.record, identity=self.identity, worker=self.absent_worker,
                      workload=self.absent_workload, containment=self.empty, domain=self.domain)
        values.update(changes)
        return e.classify_registration(**values)

    def test_positive_terminal_proof_can_override_stale_running_phase_without_changing_record(self):
        before = json.dumps(self.record, sort_keys=True)
        self.assertEqual(self.decide()['classification'], 'TERMINAL_ABSENT')
        self.assertEqual(json.dumps(self.record, sort_keys=True), before)
        self.assertEqual(self.record['containment_empty'], False)

    def test_absent_pid_without_authoritative_containment_stays_unknown(self):
        for containment in [dict(status='UNKNOWN', reason='no locator'), dict(status='ABSENT_OBSERVED'),
                            dict(status='POPULATED'), dict(status='UNKNOWN', reason='permission denied')]:
            self.assertEqual(self.decide(containment=containment)['classification'], 'UNKNOWN')

    def test_real_pid_with_different_recorded_birth_is_unknown_even_with_empty_containment(self):
        real = e.process_probe(os.getpid())
        self.assertEqual(real['status'], 'PRESENT')
        record = dict(self.record, worker_pid=os.getpid())
        identity = dict(self.identity, pid=os.getpid(), start_time_ticks=int(real['birth']) + 1)
        result = self.decide(record=record, identity=identity, worker=real)
        self.assertEqual(result['classification'], 'UNKNOWN')
        self.assertIn('reused', result['reason'])
        self.assertTrue(Path('/proc/' + str(os.getpid())).exists())

    def test_real_live_workload_prevents_terminal_worker_exclusion(self):
        live = e.process_probe(os.getpid())
        result = self.decide(record=dict(self.record, workload_pid=os.getpid()), workload=live)
        self.assertEqual(result['classification'], 'LIVE_WORKLOAD')

    def test_exact_live_worker_is_protected_despite_persisted_failed_phase(self):
        live = e.process_probe(os.getpid())
        record = dict(self.record, worker_pid=os.getpid(), phase='failed')
        identity = dict(self.identity, pid=os.getpid(), start_time_ticks=int(live['birth']))
        self.assertEqual(self.decide(record=record, identity=identity, worker=live,
                                    containment=dict(status='UNKNOWN', reason='no locator'))['classification'], 'LIVE_WORKER')

    def test_unknown_measurements_nested_visibility_and_boot_mismatch_never_become_terminal(self):
        for changes in [dict(worker=dict(status='UNKNOWN', reason='EACCES')),
                        dict(workload=dict(status='UNKNOWN', reason='EACCES')),
                        dict(domain=dict(status='UNKNOWN', reason='nested PID namespace')),
                        dict(domain=dict(self.domain, initial_pid_namespace=False)),
                        dict(identity=dict(self.identity, boot_id='other-boot')),
                        dict(identity=dict(self.identity, start_time_ticks=None))]:
            self.assertEqual(self.decide(**changes)['classification'], 'UNKNOWN')

    def test_unreadable_stat_and_missing_stat_for_existing_pid_are_unknown(self):
        real_read = Path.read_text
        for exception in [PermissionError('denied'), FileNotFoundError('stat missing')]:
            def read(path, *args, **kwargs):
                if str(path) == '/proc/%d/stat' % os.getpid():
                    raise exception
                return real_read(path, *args, **kwargs)
            with patch.object(Path, 'read_text', read):
                self.assertEqual(e.process_probe(os.getpid())['status'], 'UNKNOWN')

    def test_verified_cgroup_absence_vs_wrong_uuid_domain_symlink_and_missing_counter(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            domain = dict(self.domain, identity=dict(boot_id='fixture-boot', cgroup_root_device=root.stat().st_dev))
            locator = root / ('aplexer-workload-%s.scope' % self.sid)
            record = dict(self.record, containment_cgroup=str(locator), containment_cgroup_identity=domain['identity'])
            with patch.object(e, 'CGROUP_ROOT', tmp), patch.object(e, 'observer_domain', return_value=domain):
                self.assertEqual(e.containment_probe(record, domain)['status'], 'ABSENT_VERIFIED')
                wrong = dict(record, containment_cgroup=str(root / 'aplexer-workload-other.scope'))
                self.assertEqual(e.containment_probe(wrong, domain)['status'], 'UNKNOWN')
                self.assertEqual(e.containment_probe(record, dict(domain, identity={'boot_id':'other'}))['status'], 'UNKNOWN')
                target = root / 'unrelated'; target.mkdir()
                locator.symlink_to(target, target_is_directory=True)
                self.assertEqual(e.containment_probe(record, domain)['status'], 'UNKNOWN')
                locator.unlink();locator.mkdir()
                self.assertEqual(e.containment_probe(record, domain)['status'], 'UNKNOWN')
                (locator/'cgroup.events').write_text('populated 0\nfrozen 0\n')
                (locator/'cgroup.procs').write_text('')
                self.assertEqual(e.containment_probe(record, domain)['status'], 'EMPTY_VERIFIED')
                (locator/'cgroup.events').write_text('populated 1\nfrozen 0\n')
                (locator/'cgroup.procs').write_text(str(os.getpid()))
                self.assertEqual(e.containment_probe(record, domain)['status'], 'POPULATED')

    def test_inventory_retains_unknown_and_live_workload_and_protects_live_failed_phase(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp);directory=root/self.sid;directory.mkdir()
            record=dict(self.record);identity=dict(self.identity)
            def store():
                (directory/'session.json').write_text(json.dumps(record))
                (directory/'worker.identity.json').write_text(json.dumps(identity))
            with patch.object(e, 'observer_domain', return_value=self.domain), \
                    patch.object(e, 'process_probe', side_effect=lambda pid: self.absent_worker if pid==100 else self.absent_workload), \
                    patch.object(e, 'containment_probe', return_value=dict(status='UNKNOWN')):
                store();protected, classes, errors=e.registration_inventory(tmp)
                self.assertEqual(protected,{})
                self.assertEqual(classes[self.sid]['classification'],'UNKNOWN')
                self.assertEqual(len(errors),1)
            live=e.process_probe(os.getpid())
            record.update(worker_pid=os.getpid(),workload_pid=None,phase='failed')
            identity.update(pid=os.getpid(),start_time_ticks=int(live['birth']))
            store()
            with patch.object(e,'observer_domain',return_value=self.domain):
                protected,classes,errors=e.registration_inventory(tmp)
                self.assertIn(str(os.getpid()),protected)
                self.assertEqual(classes[self.sid]['classification'],'LIVE_WORKER')
                self.assertEqual(errors,[])
            record.update(worker_pid=100,workload_pid=os.getpid(),phase='running')
            identity.update(pid=100,start_time_ticks=123)
            store()
            with patch.object(e,'observer_domain',return_value=self.domain), \
                    patch.object(e,'process_probe',side_effect=lambda pid: self.absent_worker if pid==100 else live):
                protected,classes,errors=e.registration_inventory(tmp)
                self.assertEqual(classes[self.sid]['classification'],'LIVE_WORKLOAD')
                self.assertEqual(len(errors),1)
                self.assertNotIn('100',protected)

    def test_process_or_observer_change_during_measurement_is_unknown(self):
        with patch.object(e,'process_probe',side_effect=[self.absent_worker,self.absent_workload,
                                                      dict(status='PRESENT',pid=100,birth='456'),self.absent_workload]), \
                patch.object(e,'containment_probe',return_value=self.empty), \
                patch.object(e,'observer_domain',return_value=self.domain):
            self.assertEqual(e.inspect_registration(self.record,self.identity,self.domain)['classification'],'UNKNOWN')

    def test_postverify_rechecks_terminal_exclusion_and_refuses_lost_proof(self):
        saved = dict(classification='TERMINAL_ABSENT', source={'path':'record'},
                     identity_source={'path':'identity'}, recorded_identity=self.identity)
        before = dict(worker_registration_classifications={self.sid:saved})
        manifest = dict(committed=[],workers_before={},hooks_before={})
        for classification, expected in [('TERMINAL_ABSENT',False),('UNKNOWN',True),('LIVE_WORKLOAD',True)]:
            with patch.object(rollout,'snapshot_workers',return_value=({},[])), \
                    patch.object(rollout,'snapshot_hooks',return_value={}), \
                    patch.object(e,'read_owned_json',side_effect=[(self.record,{}),(self.identity,{})]), \
                    patch.object(e,'observer_domain',return_value=self.domain), \
                    patch.object(e,'inspect_registration',return_value={'classification':classification,'uuid':self.sid}):
                problems=rollout.post_verify(before,manifest)
                self.assertEqual(bool(problems),expected)


if __name__ == '__main__':
    unittest.main()
