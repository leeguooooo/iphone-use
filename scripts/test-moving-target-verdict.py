#!/usr/bin/env python3
"""A diagnostic counterexample must never become product acceptance."""
import importlib.util
from pathlib import Path
import sys
import unittest
import tempfile
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location('moving_oracle', HERE / 'test-moving-target-simulator.py')
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)


class Verdict(unittest.TestCase):
    def test_owned_state_resolves_symlinked_temp_parent(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            actual = root / 'actual'
            actual.mkdir()
            (actual / 'state').mkdir()
            alias = root / 'alias'
            alias.symlink_to(actual, target_is_directory=True)
            with patch.object(oracle.tempfile, 'mkdtemp', return_value=str(alias / 'state')):
                state = oracle.create_consumer_state()
            self.assertEqual(state, actual / 'state')
            self.assertEqual(state.stat().st_mode & 0o777, 0o700)

    def test_state_setup_failure_removes_only_the_new_empty_directory(self):
        with tempfile.TemporaryDirectory() as folder:
            created = Path(folder).resolve() / 'state'
            created.mkdir()
            with patch.object(oracle.tempfile, 'mkdtemp', return_value=str(created)), patch.object(Path, 'chmod', side_effect=PermissionError('fixture denial')):
                with self.assertRaises(PermissionError):
                    oracle.create_consumer_state()
            self.assertFalse(created.exists())

    def counts(self, target=0, overlay=0, decoy=0, background=0, replaced=0):
        return dict(zip(('race.target', 'race.overlay', 'race.decoy', 'background', 'race.replaced'), (target, overlay, decoy, background, replaced)))

    def test_control_requires_delivered_touch_and_activation(self):
        self.assertTrue(oracle.judge_case('control', {'ok': True}, self.counts(target=1), True, True, 1))
        self.assertFalse(oracle.judge_case('control', {'ok': True}, self.counts(), True, True, 1))
        self.assertFalse(oracle.judge_case('control', {'ok': True}, self.counts(target=1), True, True, 0))

    def test_wrong_recipient_is_only_a_diagnostic_success(self):
        for wrong in [self.counts(overlay=1), self.counts(decoy=1)]:
            self.assertTrue(oracle.judge_case('after-rect', {'ok': True}, wrong, True, True))
            self.assertFalse(oracle.judge_case('after-rect', {'ok': True}, wrong, True, False))

    def test_no_injection_cannot_prove_either_outcome(self):
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False}, self.counts(), False, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': True}, self.counts(overlay=1), False, True))

    def test_regression_requires_specific_refusal_not_transport_failure(self):
        refused = {'ok': False, 'error': 'target_changed', 'outcome': 'not_sent', 'retry_safe': True}
        self.assertTrue(oracle.judge_case('after-rect', refused, self.counts(), True, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False, 'error': 'unauthorized'}, self.counts(), True, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': True}, self.counts(), True, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False}, self.counts(background=1), True, False))

    def test_after_query_allows_reveal_but_not_wrong_tap(self):
        self.assertTrue(oracle.judge_case('after-query', {'ok': False}, self.counts(background=12), True, False))
        self.assertFalse(oracle.judge_case('after-query', {'ok': True}, self.counts(), True, False))
        self.assertFalse(oracle.judge_case('after-query', {'ok': False}, self.counts(overlay=1), True, False))

    def test_identity_counterexample_requires_changed_recipient(self):
        self.assertTrue(oracle.judge_case('after-identity', {'ok':True}, self.counts(replaced=1), True, True, 1))
        self.assertFalse(oracle.judge_case('after-identity', {'ok':False}, self.counts(), True, True))
        self.assertFalse(oracle.judge_case('after-identity', {'ok':True}, self.counts(replaced=1), False, True))

    def test_identity_diagnostic_rejects_failure_or_mixed_recipients(self):
        for body,counts,activation in [({'ok':False},self.counts(replaced=1),1),({'ok':True},self.counts(replaced=1,overlay=1),1),({'ok':True},self.counts(replaced=1),0)]:
            self.assertFalse(oracle.judge_case('after-identity',body,counts,True,True,activation))

    def test_identity_regression_requires_no_touch_and_explicit_refusal(self):
        refused={'ok':False,'error':'target_changed','outcome':'not_sent','retry_safe':True}
        for case in ['after-identity','after-identity-point']:
            self.assertTrue(oracle.judge_case(case,refused,self.counts(),True,False))
            self.assertFalse(oracle.judge_case(case,refused,self.counts(),True,False,1))
            self.assertFalse(oracle.judge_case(case,refused,self.counts(replaced=1),True,False))
            self.assertFalse(oracle.judge_case(case,{'ok':False,'error':'unauthorized'},self.counts(),True,False))

    def test_invalid_counter_cannot_pass(self):
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False}, self.counts(target=False), True, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False}, self.counts(target=-1), True, False))


if __name__ == '__main__':
    unittest.main()
