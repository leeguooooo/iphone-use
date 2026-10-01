#!/usr/bin/env python3
"""The nightly canary leaves a person's phone and a parked phone alone.

A canary that takes back a handed-over phone ends someone's session; one that
brings a parked phone up makes iOS ask for the passcode at 03:30.
"""
import importlib.util
import pathlib

MODULE = pathlib.Path(__file__).with_name("flow-reverify.py")
spec = importlib.util.spec_from_file_location("flow_reverify", MODULE)
reverify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reverify)

calls = []
reverify.log = lambda msg: None


def fake_http(status):
    def http(method, path, body=None, control=False, timeout=60):
        calls.append((method, path, body))
        return 200, status
    return http


# Handed to a person: preflight skips and never touches the device.
calls.clear()
reverify.http = fake_http({"human_handoff": True, "device_state": "released", "released": True})
st, why = reverify.preflight()
assert st is None and why == "phone handed to a person", (st, why)
assert calls == [("GET", "/agent/status", None)], calls

# Parked phone, default: no bring-up request at all.
calls.clear()
reverify.BRING_UP = False
assert reverify.bring_up({"drivable": False, "device_state": "released"}) is False
assert calls == [], calls

# Parked phone, opted in: one bring-up request.
calls.clear()
reverify.BRING_UP = True
reverify.time.sleep = lambda s: None
reverify.http = fake_http({"drivable": True})
assert reverify.bring_up({"drivable": False, "device_state": "offline"}) is True
assert calls[0] == ("POST", "/agent/mode", {"mode": "agent"}), calls

print("ok: the canary skips a handed-over phone and does not wake a parked one")
