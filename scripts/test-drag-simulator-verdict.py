#!/usr/bin/env python3
"""Device-free checks of the simulator gesture acceptance verdict."""
import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
import json
import tempfile
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("drag_check", Path(__file__).with_name("test-drag-simulator.py"))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class VerdictTests(unittest.TestCase):
    def test_scroll_ack_without_the_right_selection_is_not_a_pass(self):
        record = {"run_id": "fresh", "selections": [8], "offset_y": 439}
        self.assertEqual(check.judge_input(record, "fresh", "scroll", 8), [])
        for value in [None, {"ok": True}, dict(record, run_id="old"),
                      dict(record, selections=[]), dict(record, selections=[7]),
                      dict(record, selections=[8, 8]), dict(record, offset_y=0),
                      dict(record, offset_y=float("nan"))]:
            self.assertTrue(check.judge_input(value, "fresh", "scroll", 8), value)

    def test_first_focus_typing_requires_the_actual_text_receiver(self):
        record = {"run_id": "fresh", "text": "照片", "began_editing": 1, "selections": []}
        self.assertEqual(check.judge_input(record, "fresh", "typing", 3), [])
        for key, value in [("text", ""), ("text", "照"), ("text", "照片照片"), ("began_editing", 0),
                           ("began_editing", True), ("selections", [3]), ("run_id", "old")]:
            self.assertTrue(check.judge_input(dict(record, **{key: value}), "fresh", "typing", 3))

    def test_multi_hold_requires_three_simultaneous_stationary_full_duration_touches(self):
        record = {"run_id": "fresh", "down_count": 3, "up_count": 3, "cancel_count": 0,
                  "max_fingers": 3, "holds": [{"start": [x, 0.4], "end": [x, 0.4], "duration_ms": 1500}
                                              for x in (0.3, 0.5, 0.7)]}
        self.assertEqual(check.judge(record, "fresh", False, "multi-flow"), [])
        for key, value in [("max_fingers", 1), ("up_count", 1), ("cancel_count", 1),
                           ("run_id", "old"), ("holds", [])]:
            self.assertTrue(check.judge(dict(record, **{key: value}), "fresh", False, "multi-flow"))
        for key, value in [("duration_ms", 50), ("duration_ms", True), ("end", [0.5, 0.4]),
                           ("start", [float("nan"), 0.4])]:
            bad = copy.deepcopy(record)
            bad["holds"][0][key] = value
            self.assertTrue(check.judge(bad, "fresh", False, "multi-single"))
        bad = copy.deepcopy(record)
        bad["holds"][1] = bad["holds"][0]
        self.assertTrue(check.judge(bad, "fresh", False, "multi-batch"))

    def record(self):
        return {"run_id": "fresh", "down_count": 1, "up_count": 1, "cancel_count": 0,
                "max_fingers": 1, "hover_ms": 900, "duration_ms": 1900, "initial_hold_ms": 400,
                "start": [0.3, 0.4], "end": [0.5, 0.4], "visits": ["start", "edge", "end"]}

    def test_accepts_observed_single_finger_hold(self):
        self.assertEqual(check.judge(self.record(), "fresh", True), [])

    def test_ack_or_empty_ui_is_never_a_pass(self):
        for value in [None, {}, {"ok": True}]:
            self.assertTrue(check.judge(value, "fresh", True))

    def test_stale_replayed_cancelled_and_wrong_target_events_fail(self):
        for key, value in [("run_id", "old"), ("down_count", 2), ("up_count", 2),
                           ("cancel_count", 1), ("max_fingers", 2), ("hover_ms", 200), ("initial_hold_ms", 100),
                           ("initial_hold_ms", 1000), ("duration_ms", 1333),
                           ("visits", ["start", "end"]), ("end", [0.13, 0.5]),
                           ("end", [float("nan"), 0.4]), ("start", None), ("down_count", True)]:
            record = copy.deepcopy(self.record())
            record[key] = value
            self.assertTrue(check.judge(record, "fresh", True), (key, value))

    def test_slow_drift_is_a_required_negative_control(self):
        record = self.record()
        self.assertTrue(check.judge(record, "fresh", False))
        record["hover_ms"] = 250
        record["duration_ms"] = 3100
        self.assertEqual(check.judge(record, "fresh", False), [])
        self.assertTrue(check.judge(record, "fresh", True))

    def test_longpress_is_not_satisfied_by_a_short_tap(self):
        record = self.record()
        record.update({"visits": ["start"], "end": [0.3, 0.4], "hover_ms": 0,
                       "initial_hold_ms": None, "duration_ms": 600})
        self.assertEqual(check.judge(record, "fresh", False, kind="longpress"), [])
        record["duration_ms"] = 50
        self.assertTrue(check.judge(record, "fresh", False, kind="longpress"))

    def test_native_swipe_requires_real_movement_time(self):
        record = self.record()
        record.update({"visits": ["start", "end"], "hover_ms": 0,
                       "initial_hold_ms": 33, "duration_ms": 716})
        self.assertEqual(check.judge(record, "fresh", False, kind="native"), [])
        record["duration_ms"] = 16
        self.assertTrue(check.judge(record, "fresh", False, kind="native"))

    def test_wrong_daemon_target_or_owner_refused_before_a_gesture(self):
        daemon = check.Daemon("http://127.0.0.1:44321", "fixture")
        for status in [{"target": "device", "udid": "sim", "drivable": True},
                       {"target": "simulator", "udid": "other", "drivable": True},
                       {"target": "simulator", "udid": "sim", "drivable": False},
                       {"target": "simulator", "udid": "sim", "drivable": True, "owner": "other"}]:
            daemon.call = lambda path, value=status: value
            with self.assertRaises(RuntimeError):
                daemon.require_simulator("sim")

    def test_native_path_refuses_another_simulator_record_before_reading_auth(self):
        with tempfile.TemporaryDirectory() as root:
            record = Path(root) / "simulator.json"
            args = SimpleNamespace(runner_record=record, udid="ours")
            for body in [{"udid":"other","port":8200}, {"udid":"ours","port":0}]:
                record.write_text(json.dumps(body))
                with patch.object(check.urllib.request, "urlopen") as opened:
                    with self.assertRaises(RuntimeError):
                        check.runner_call(args, "/swipe", {})
                    opened.assert_not_called()

    def test_localized_settings_label_is_bound_to_bundle_and_source_pid(self):
        with tempfile.TemporaryDirectory() as root:
            args = SimpleNamespace(udid="ours", settings_suite=Path(root)/"suite.json")
            daemon = SimpleNamespace(require_simulator=lambda _: None, call=lambda *_: {"ok":True})
            active = ({"value":{"bundleId":"com.apple.Preferences","pid":123}}, {})
            source = ({"value":{"type":"XCUIElementTypeApplication","label":"Localized settings"}}, {"X-IPU-Pid":"123"})
            with patch.object(check, "runner_call", side_effect=[active, source]):
                binding = check.localized_settings_suite(daemon, args)
            self.assertEqual(binding["bundle"], "com.apple.Preferences")
            self.assertEqual(json.loads(args.settings_suite.read_text())["cases"][0]["assert"][0]["application"], "Localized settings")

    def test_same_label_from_another_app_or_pid_is_not_accepted(self):
        for bundle, pid in [("com.example.wrong", "123"), ("com.apple.Preferences", "999")]:
            with tempfile.TemporaryDirectory() as root:
                args = SimpleNamespace(udid="ours", settings_suite=Path(root)/"suite.json")
                daemon = SimpleNamespace(require_simulator=lambda _: None, call=lambda *_: {"ok":True})
                active = ({"value":{"bundleId":bundle,"pid":123}}, {})
                source = ({"value":{"type":"XCUIElementTypeApplication","label":"Settings"}}, {"X-IPU-Pid":pid})
                with patch.object(check, "runner_call", side_effect=[active, source]), patch.object(check.time, "monotonic", side_effect=[0,0,16]), patch.object(check.time, "sleep"):
                    with self.assertRaises(RuntimeError):
                        check.localized_settings_suite(daemon, args)
                self.assertFalse(args.settings_suite.exists())

    def test_compatibility_sized_fixture_is_refused_before_input(self):
        check.require_matching_viewport({"width":393,"height":852}, {"value":{"width":393,"height":852}})
        for fixture in [{"width":390,"height":844}, {"width":float("nan"),"height":852}, {}]:
            with self.assertRaises(RuntimeError):
                check.require_matching_viewport(fixture, {"value":{"width":393,"height":852}})

    def test_remote_and_ambiguous_urls_are_refused(self):
        for base in ["http://example.com:44321", "https://127.0.0.1", "http://user@localhost", "http://localhost?x=1"]:
            with self.assertRaises(ValueError):
                check.Daemon(base, "fixture")


if __name__ == "__main__":
    unittest.main()
