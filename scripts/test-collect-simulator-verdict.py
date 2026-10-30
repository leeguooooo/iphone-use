#!/usr/bin/env python3
"""Offline negative controls for the independent Native collection oracle."""
import copy
import importlib.util
from pathlib import Path
import sys
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location('list_oracle', HERE / 'test-collect-simulator.py')
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def sample(mode='unique'):
    oracle = {'run_id': 'fresh-fixture', 'mode': mode, 'rows': probe.fixture_model(mode)}
    before = {'run_id': oracle['run_id'], 'mode': mode, 'pid': 123, 'offset_y': 0}
    after = dict(before, max_offset_y=1500, scroll_events=10, seen_indices=list(range(len(oracle['rows']))))
    rows = [{**row, 'page': 1 if row['index'] < 12 else 2} for row in oracle['rows']]
    result = {'ok': True, 'rows': rows, 'row_count': len(rows), 'pages': [{}, {}],
              'swipes': 1, 'swipes_confirmed': 1, 'swipes_uncertain': 0, 'snapshot': 'last-page',
              'complete': True, 'end_label_seen': True, 'stop_reason': 'end_label'}
    if mode == 'missing':
        result.update(rows=rows[:1], row_count=1, coverage='absent or reused identifiers with identical content may collapse', complete_scope='end_label_visibility', business_complete=False)
    if mode == 'counter':
        result.update(complete=False, end_label_seen=False, stop_reason='duplicate_page')
        after.update(counter_changed=True, pan_events=1, offset_y=0, max_offset_y=0, max_abs_offset_y=0)
    seed = {'elements': [{**row, 'kind': 'Cell'} for row in oracle['rows']]}
    last = {'snapshot': 'last-page', 'elements': copy.deepcopy(seed['elements'])}
    if mode == 'counter':
        for row in last['elements']: row['value'] = 'after'
    return [oracle, before, after, result, last, seed]


class OracleTests(unittest.TestCase):
    def test_four_declared_contracts(self):
        for mode in probe.MODES:
            with self.subTest(mode=mode):
                self.assertEqual(probe.verify_case(*sample(mode)), [])

    def test_anonymous_list_requires_end_and_independent_index_coverage(self):
        for fault in ('early-stop', 'missing-index', 'missing-limitation', 'business-complete', 'legacy-inconsistent'):
            values = sample('missing')
            if fault == 'early-stop': values[3].update(end_label_seen=False, stop_reason='no_progress')
            elif fault == 'missing-index': values[2]['seen_indices'].pop()
            elif fault == 'missing-limitation': values[3]['coverage'] = ''
            elif fault == 'business-complete': values[3]['business_complete'] = True
            else: values[3]['complete'] = False
            with self.subTest(fault=fault): self.assertTrue(probe.verify_case(*values))

    def test_old_text_only_collapse_is_not_complete_coverage(self):
        values = sample(); values[3].update(rows=values[3]['rows'][:1], row_count=1)
        self.assertTrue(probe.verify_case(*values))

    def test_missing_duplicate_extra_and_changed_rows_fail(self):
        for fault in ('missing', 'duplicate', 'extra', 'value', 'identifier'):
            values = sample(); rows = values[3]['rows']
            if fault == 'missing': rows.pop()
            elif fault == 'duplicate': rows[-1] = copy.deepcopy(rows[0])
            elif fault == 'extra': rows.append(copy.deepcopy(rows[0]))
            else: rows[0][fault] = 'wrong'
            values[3]['row_count'] = len(rows)
            with self.subTest(fault=fault): self.assertTrue(probe.verify_case(*values))

    def test_ack_without_ui_movement_is_rejected(self):
        values = sample(); values[2].update(max_offset_y=0, scroll_events=0)
        self.assertTrue(probe.verify_case(*values))

    def test_unknown_receipt_and_missing_last_snapshot_fail(self):
        for fault in ('uncertain', 'snapshot', 'page', 'pid', 'nonce', 'oracle', 'coverage'):
            values = sample()
            if fault == 'uncertain': values[3]['swipes_uncertain'] = 1
            elif fault == 'snapshot': values[4]['snapshot'] = 'different-page'
            elif fault == 'page': values[3]['rows'][0]['page'] = 99
            elif fault == 'pid': values[2]['pid'] = 999
            elif fault == 'nonce': values[2]['run_id'] = 'stale'
            elif fault == 'oracle': values[0]['rows'] = []
            else: values[3]['complete'] = False
            with self.subTest(fault=fault): self.assertTrue(probe.verify_case(*values))

    def test_missing_identifier_fallback_must_not_claim_full_count(self):
        values = sample('missing'); values[3]['rows'] *= 32; values[3]['row_count'] = 32
        self.assertTrue(probe.verify_case(*values))

    def test_success_shaped_payload_does_not_hide_transport_errors(self):
        success = {'result': {'isError': False, 'structuredContent': sample()[3]}}
        self.assertEqual(probe.transport_errors(mcp_reply=success), [])
        success['result']['isError'] = True
        self.assertTrue(probe.transport_errors(mcp_reply=success))
        self.assertTrue(probe.transport_errors(mcp_reply={'error': {'code': -1}, **success}))
        self.assertTrue(probe.transport_errors(http_status=409))
        self.assertTrue(probe.transport_errors(http_status=500))
        self.assertEqual(probe.transport_errors(http_status=200), [])

    def test_counter_stale_native_values_missing_seed_and_transient_movement_fail(self):
        for fault in ('stale-native', 'missing-seed', 'moved-back'):
            values = sample('counter')
            if fault == 'stale-native':
                for row in values[4]['elements']: row['value'] = 'before'
            elif fault == 'missing-seed': values[5]['elements'] = []
            else: values[2]['max_abs_offset_y'] = 50
            with self.subTest(fault=fault): self.assertTrue(probe.verify_case(*values))

    def test_mcp_device_gate_refuses_wrong_target_before_collection(self):
        status = {'ok': True, 'target': 'simulator', 'udid': 'owned-uuid', 'drivable': True, 'owner': None}
        self.assertEqual(probe.target_errors(status, 'OWNED-UUID', 'test-owner'), [])
        for key, value in [('target', 'iphone'), ('udid', 'other-uuid'), ('drivable', False), ('owner', 'other-owner'), ('ok', False)]:
            wrong = {**status, key: value}
            with self.subTest(field=key): self.assertTrue(probe.target_errors(wrong, 'owned-uuid', 'test-owner'))

    def test_counter_requires_observed_refresh_and_first_values(self):
        for fault in ('not-observed', 'latest-values', 'continued'):
            values = sample('counter')
            if fault == 'not-observed': values[2]['counter_changed'] = False
            elif fault == 'latest-values': values[3]['rows'][0]['value'] = 'after'
            else: values[3]['stop_reason'] = 'max_pages'
            with self.subTest(fault=fault): self.assertTrue(probe.verify_case(*values))


if __name__ == '__main__':
    unittest.main()
