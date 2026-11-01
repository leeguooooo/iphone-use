#!/usr/bin/env python3
"""A diagnostic counterexample must never become product acceptance."""
import importlib.util
from pathlib import Path
import sys
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location('moving_oracle', HERE / 'test-moving-target-simulator.py')
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)


class Verdict(unittest.TestCase):
    def counts(self, target=0, overlay=0, decoy=0, background=0):
        return dict(zip(('race.target', 'race.overlay', 'race.decoy', 'background'), (target, overlay, decoy, background)))

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

    def test_guard_acceptance_refuses_all_touches_and_false_success(self):
        self.assertTrue(oracle.judge_case('after-rect', {'ok': False}, self.counts(), True, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': True}, self.counts(), True, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False}, self.counts(background=1), True, False))

    def test_after_query_allows_reveal_but_not_wrong_tap(self):
        self.assertTrue(oracle.judge_case('after-query', {'ok': False}, self.counts(background=12), True, False))
        self.assertFalse(oracle.judge_case('after-query', {'ok': True}, self.counts(), True, False))
        self.assertFalse(oracle.judge_case('after-query', {'ok': False}, self.counts(overlay=1), True, False))

    def test_invalid_counter_cannot_pass(self):
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False}, self.counts(target=False), True, False))
        self.assertFalse(oracle.judge_case('after-rect', {'ok': False}, self.counts(target=-1), True, False))


if __name__ == '__main__':
    unittest.main()
