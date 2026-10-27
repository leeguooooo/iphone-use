#!/usr/bin/env python3
"""Regression verdicts against retained physical receipts, including real misclicks."""
import copy
import gzip
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch, Mock
from types import SimpleNamespace

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('recovery_verdict',ROOT/'scripts/test-replacement-recovery-simulator.py')
recovery=importlib.util.module_from_spec(spec);spec.loader.exec_module(recovery)
FOLDER=ROOT/'scripts/fixtures/replacement-recovery'

class VerdictTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        data=json.loads(gzip.decompress((FOLDER/'results.json.gz').read_bytes()))
        cls.records={r['trial']:r for group in data['cohorts'].values() for r in group['cases']}

    def verdict(self,name,record=None):
        r=record or self.records[name]
        return recovery.judge(name,r['before'],r['after'],r['reply'])

    def test_final_receipts_satisfy_their_distinct_contracts(self):
        for case in recovery.CASES:
            with self.subTest(case=case[0]):self.assertEqual(self.verdict(case[0]),[])

    def test_actual_rejected_overlay_misclick_is_rejected(self):
        data=json.loads(gzip.decompress((FOLDER/'retained-trials.json.gz').read_bytes()))
        found=[]
        for lane in data['retained-v5'].values():
            for r in lane['cases']:
                if r['trial']=='covered-original':found.append(r)
        self.assertTrue(found)
        for r in found:self.assertIn('overlay action was activated',self.verdict('covered-original',r))

    def test_no_credit_when_fault_never_triggered(self):
        r=copy.deepcopy(self.records['advance-opt-in-1']);r['after']['advance_triggered']=False
        self.assertTrue(self.verdict('advance-opt-in-1',r))
        r=copy.deepcopy(self.records['refilled-on-focus']);r['after']['a_touch_starts']=r['after']['a_touches_at_clear']
        self.assertTrue(self.verdict('refilled-on-focus',r))

    def test_completion_ack_cannot_hide_missing_text_or_changed_other_fields(self):
        r=copy.deepcopy(self.records['normal-opt-in']);r['after']['fields']['a']['sha256']=recovery.digest('字'*400)
        self.assertIn('requested replacement not exact',self.verdict('normal-opt-in',r))
        r=copy.deepcopy(self.records['normal-opt-in']);r['after']['fields']['b']['sha256']=recovery.digest('wrong')
        self.assertTrue(self.verdict('normal-opt-in',r))

    def test_partial_input_must_not_be_promoted_and_safer_segment_can_improve(self):
        r=copy.deepcopy(self.records['late-focus']);r['reply']['result']['structuredContent']['outcome']='applied'
        self.assertTrue(self.verdict('late-focus',r))
        r=copy.deepcopy(self.records['late-focus']);r['after']['fields']['b']['characters']=0;r['after']['fields']['b']['sha256']=recovery.digest('')
        self.assertEqual(self.verdict('late-focus',r),[])
        r=copy.deepcopy(self.records['late-focus']);r['after']['fields']['a']['sha256']=recovery.digest('shift-focus-test-'+'字'*483)
        r['reply']['result']['structuredContent']['outcome']='applied'
        self.assertTrue(self.verdict('late-focus',r),"a complete original cannot hide text leaked to B")

class IdleGateTests(unittest.TestCase):
    def setUp(self):
        self.args=SimpleNamespace(owner='test',udid='owned-simulator')
        self.daemon=Mock()
        self.daemon.require_simulator.return_value={'owner':'test','screen_private_secs':0}
        self.clock=0.0

    def sleep(self,seconds):self.clock+=seconds

    def run_gate(self,responses,peer=None,timeout=.3):
        with patch.object(recovery.drag,'runner_call',side_effect=responses) as call, \
             patch.object(recovery.time,'monotonic',side_effect=lambda:self.clock), \
             patch.object(recovery.time,'sleep',side_effect=self.sleep), patch('builtins.print'):
            value=recovery.wait_for_native_idle(self.daemon,self.args,peer,timeout=timeout)
        return value,call

    def recovering_status(self):
        return {'target':'simulator','udid':self.args.udid,'owner':self.args.owner,
            'screen_private_secs':0,'human_handoff':False,'released':False,
            'reconnecting':False,'runner':True,'runner_locked':False,
            'drivable':False,'device_state':'degraded'}

    def test_degraded_read_path_waits_on_metadata_before_resuming_strict_gate(self):
        self.daemon.require_simulator.side_effect=[RuntimeError('simulator is not drivable'),
            {'owner':'test','screen_private_secs':0}]
        self.daemon.call.return_value=self.recovering_status()
        value,call=self.run_gate([({'value':{'busy':False}}, {})])
        self.assertIs(value['value']['busy'],False)
        self.assertEqual(self.clock,.2)
        self.daemon.call.assert_called_once_with('/agent/status')
        self.assertEqual(call.call_count,1)
        self.assertEqual(self.daemon.require_simulator.call_count,2)

    def test_degraded_read_path_has_the_same_bounded_budget_and_no_native_read(self):
        self.daemon.require_simulator.side_effect=RuntimeError('simulator is not drivable')
        self.daemon.call.return_value=self.recovering_status()
        with patch.object(recovery.drag,'runner_call') as call, \
             patch.object(recovery.time,'monotonic',side_effect=lambda:self.clock), \
             patch.object(recovery.time,'sleep',side_effect=self.sleep), patch('builtins.print'):
            with self.assertRaisesRegex(RuntimeError,'read path did not become drivable'):
                recovery.wait_for_native_idle(self.daemon,self.args,timeout=.3)
        self.assertEqual(self.clock,.3)
        call.assert_not_called()
        self.assertTrue(all(c.args==('/agent/status',) for c in self.daemon.call.call_args_list))

    def test_read_recovery_never_waits_through_scope_lock_or_device_changes(self):
        for changes in [{'owner':'other'},{'screen_private_secs':1},{'udid':'other'},
                        {'target':'phone'},{'human_handoff':True},{'released':True},
                        {'reconnecting':True},{'runner':False},{'runner_locked':True},
                        {'device_state':'locked'},{'device_state':'read_failing'},{'runner_locked':None},{'drivable':'false'}]:
            with self.subTest(changes=changes):
                self.daemon.require_simulator.side_effect=RuntimeError('simulator is not drivable')
                self.daemon.call.return_value=dict(self.recovering_status(),**changes)
                with patch.object(recovery.drag,'runner_call') as call:
                    with self.assertRaisesRegex(RuntimeError,'simulator is not drivable'):
                        recovery.wait_for_native_idle(self.daemon,self.args)
                call.assert_not_called()

    def test_busy_startup_can_settle_before_any_reset(self):
        value,call=self.run_gate([({'value':{'busy':True}},{}),({'value':{'busy':False}}, {})])
        self.assertIs(value['value']['busy'],False)
        self.assertEqual(call.call_count,2)
        self.assertEqual(self.daemon.require_simulator.call_count,2)
        self.assertEqual(self.clock,.2)
        self.assertTrue(all(c.args[1]=='/status' and len(c.args)==2 for c in call.call_args_list))

    def test_pending_mcp_never_polls_or_advances(self):
        with patch.object(recovery.drag,'runner_call') as call:
            with self.assertRaisesRegex(RuntimeError,'MCP request remains pending'):
                recovery.wait_for_native_idle(self.daemon,self.args,SimpleNamespace(pending=1))
        call.assert_not_called()
        self.daemon.require_simulator.assert_not_called()

    def test_permanent_busy_stops_at_budget_without_an_input_request(self):
        with patch.object(recovery.drag,'runner_call',return_value=({'value':{'busy':True}},{})) as call, \
             patch.object(recovery.time,'monotonic',side_effect=lambda:self.clock), \
             patch.object(recovery.time,'sleep',side_effect=self.sleep), patch('builtins.print'):
            with self.assertRaisesRegex(RuntimeError,'did not become idle'):
                recovery.wait_for_native_idle(self.daemon,self.args,timeout=.3)
        self.assertEqual(self.clock,.3)
        self.assertTrue(all(c.args[1]=='/status' and len(c.args)==2 for c in call.call_args_list))

    def test_missing_or_nonboolean_status_cannot_authorize_reset(self):
        for busy in [None,0,1,'false']:
            with self.subTest(busy=busy),self.assertRaisesRegex(RuntimeError,'idle status unavailable'):
                self.run_gate([({'value':{'busy':busy}}, {})])

    def test_owner_or_privacy_change_stops_before_next_native_poll(self):
        for changed in [{'owner':'other','screen_private_secs':0},{'owner':'test','screen_private_secs':1}]:
            self.daemon.require_simulator.side_effect=[{'owner':'test','screen_private_secs':0},changed]
            self.clock=0
            with patch.object(recovery.drag,'runner_call',return_value=({'value':{'busy':True}},{})) as call, \
                 patch.object(recovery.time,'monotonic',side_effect=lambda:self.clock), \
                 patch.object(recovery.time,'sleep',side_effect=self.sleep), patch('builtins.print'):
                with self.assertRaisesRegex(RuntimeError,'existing nonprivate Simulator lease'):
                    recovery.wait_for_native_idle(self.daemon,self.args,timeout=.3)
            self.assertEqual(call.call_count,1)


if __name__=='__main__':unittest.main()
