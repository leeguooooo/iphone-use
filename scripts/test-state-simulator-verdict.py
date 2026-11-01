#!/usr/bin/env python3
"""Reject stale, unknown and mistyped Native state oracle receipts."""
import ast
import copy
import hashlib
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


class TextState(unittest.TestCase):
    def sample(self, text):
        return {'touches': 0, 'return_events': 0, 'controls': {
            'state.field': {'focused': True, 'characters': len(text),
                            'utf16_length': len(text.encode('utf-16-le')) // 2,
                            'sha256': hashlib.sha256(text.encode()).hexdigest()},
            'state.other': {'focused': False, 'sha256': hashlib.sha256(b'untouched').hexdigest()}}}

    def test_native_input_payload_contains_private_use_scalars(self):
        module = ast.parse((HERE / 'test-state-simulator.py').read_text())
        literals = [ast.literal_eval(node.value) for node in ast.walk(module)
                    if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name)
                    and target.id == 'literal' for target in node.targets)]
        self.assertEqual(len(literals), 1)
        self.assertIn(chr(0xE007), literals[0])
        self.assertIn(chr(0xE003), literals[0])
        self.assertNotIn('\\ue007', literals[0])

    def test_full_unicode_hash_and_both_length_units_agree(self):
        for text in ('', '字' * 500, 'literal-\ue007-\ue003-👋🏻'):
            self.assertTrue(oracle.text_state_matches(self.sample(text), text))
            self.assertFalse(oracle.text_state_matches(self.sample(text), text + 'extra'))

    def test_wrong_field_return_touch_and_unknown_metadata_fail(self):
        for field, key, wrong in [('state.field', 'focused', False),
                                 ('state.field', 'characters', True),
                                 ('state.field', 'utf16_length', 2),
                                 ('state.field', 'sha256', '0' * 64),
                                 ('state.other', 'focused', True),
                                 ('state.other', 'sha256', 'changed')]:
            value = self.sample('👋🏻')
            value['controls'][field][key] = wrong
            self.assertFalse(oracle.text_state_matches(value, '👋🏻'), (field, key))
        for key, wrong in [('touches', 1), ('return_events', 1), ('touches', False), ('return_events', None)]:
            value = self.sample('x');value[key] = wrong
            self.assertFalse(oracle.text_state_matches(value, 'x'))
        for bad in (None, {}, {'controls': []}, {'controls': {'state.field': None}}):
            self.assertFalse(oracle.text_state_matches(bad, 'x'))


class ContinuationConsumer(unittest.TestCase):
    def run_case(self, responses):
        module = ast.parse((HERE / 'test-state-simulator.py').read_text())
        function = next(node for node in ast.walk(module) if isinstance(node, ast.FunctionDef) and node.name == 'complete_text')
        requests, inspected = [], []
        def call(action, label):
            requests.append(action.copy())
            return responses.pop(0)
        def inspect(text):
            inspected.append(text)
            return {'verified_text': text}
        namespace = {'call_text': call, 'inspect_text': inspect, 'save': lambda *_: None, 're': __import__('re')}
        exec(compile(ast.Module(body=[function], type_ignores=[]), '<production-consumer>', 'exec'), namespace)
        return namespace['complete_text'], requests, inspected

    def test_cleared_but_unsent_text_continues_without_clearing_again(self):
        text, token = 'literal-' + chr(0xE007), 'a' * 64
        complete, calls, inspected = self.run_case([
            {'ok': False, 'error': 'text_partially_typed', 'characters_confirmed': 0,
             'characters_uncertain': 0, 'continuation_safe': True, 'continue_token': token, 'remaining_text': text},
            {'ok': True}])
        result, count, consumed = complete(text, {'type': 'text', 'text': text, 'clear': True}, 'literal')
        self.assertEqual(count, 1)
        self.assertEqual(consumed, token)
        self.assertEqual(calls, [{'type': 'text', 'text': text, 'clear': True}, {'type': 'text', 'continue_token': token}])
        self.assertEqual(inspected, ['', text])
        self.assertEqual(result['verified_text'], text)

    def test_uncertain_delivery_never_dispatches_a_continuation(self):
        complete, calls, _ = self.run_case([{'ok': False, 'error': 'text_partially_typed',
            'characters_confirmed': 0, 'characters_uncertain': 1, 'continuation_safe': False}])
        with self.assertRaises(AssertionError):
            complete('x', {'type': 'text', 'text': 'x'}, 'unknown')
        self.assertEqual(len(calls), 1)


if __name__ == '__main__':
    unittest.main()
