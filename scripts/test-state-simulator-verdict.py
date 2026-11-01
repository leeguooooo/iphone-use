#!/usr/bin/env python3
"""Reject stale, unknown and mistyped Native state oracle receipts."""
import copy
import importlib.util
from pathlib import Path
import sys
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location('state_oracle', HERE / 'test-state-simulator.py')
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)


class Verdict(unittest.TestCase):
    def sample(self, expected=False):
        return {'run_id': 'owned', 'pid': 123, 'application_state': 0, 'key_window': True,
            'width': 390, 'height': 844, 'visible_indices': [0], 'touches': 0,
            'controls': {'field': {'focused': expected}}}

    def judge(self, before, after, expected, value):
        return oracle.judge(before, after, 'owned', 123, 'field', 'focused', expected, value)

    def test_known_true_and_false_require_agreement(self):
        for expected in (True, False):
            sample = self.sample(expected)
            self.assertTrue(self.judge(sample, sample, expected, expected))
            self.assertFalse(self.judge(sample, sample, expected, not expected))

    def test_unknown_and_numeric_values_are_not_known_booleans(self):
        sample = self.sample()
        for value in (None, 0, 1, 'false', [], {}):
            self.assertFalse(self.judge(sample, sample, False, value))

    def test_stale_foreground_touch_and_app_truth_fail(self):
        for key, value in [('run_id', 'old'), ('pid', 999), ('application_state', 2),
                           ('key_window', False), ('touches', 1), ('touches', False),
                           ('controls', {'field': {'focused': 0}}), ('controls', {})]:
            before = self.sample()
            after = copy.deepcopy(before)
            after[key] = value
            self.assertFalse(self.judge(before, after, False, False), key)
            self.assertFalse(self.judge(after, before, False, False), key)


class FocusReceipt(unittest.TestCase):
    def sample(self, wanted=True):
        return {'command_sequence': 1,
                'focus_command': {'sequence': 1, 'requested': wanted, 'acknowledged': True,
                                  'before': not wanted, 'after': wanted},
                'controls': {'state.field': {'focused': wanted}}}

    def test_api_acknowledgement_and_current_state_are_both_required(self):
        for wanted in (True, False):
            value = self.sample(wanted)
            self.assertTrue(oracle.focus_confirmed(value, 1, wanted))
            value['focus_command']['acknowledged'] = False
            self.assertFalse(oracle.focus_confirmed(value, 1, wanted))
            value['focus_command']['acknowledged'] = True
            value['controls']['state.field']['focused'] = not wanted
            self.assertFalse(oracle.focus_confirmed(value, 1, wanted))

    def test_stale_missing_and_mistyped_receipts_never_confirm(self):
        for key, wrong in [('sequence', 0), ('sequence', True), ('requested', False),
                           ('requested', 1), ('acknowledged', 1), ('acknowledged', None),
                           ('before', None), ('after', 0)]:
            value = self.sample()
            value['focus_command'][key] = wrong
            self.assertFalse(oracle.focus_confirmed(value, 1, True), (key, wrong))
        for wrong in (None, {}, [], {'command_sequence': 1},
                      {'command_sequence': 1, 'controls': []}):
            self.assertFalse(oracle.focus_confirmed(wrong, 1, True))
        value = self.sample()
        value['command_sequence'] = True
        self.assertFalse(oracle.focus_confirmed(value, 1, True))


if __name__ == '__main__':
    unittest.main()
