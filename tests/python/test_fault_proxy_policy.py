#!/usr/bin/env python3
"""Offline actual-handler regressions, no HTTP endpoints, keys or signed transactions."""
import importlib.util
import io
import json
from pathlib import Path
import threading
import types
import unittest

spec = importlib.util.spec_from_file_location("fault_proxy", Path(__file__).resolve().parents[2] / 'tests/runners/testnet-fault-proxy.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def held_then_readonly(mode, during_identity=False):
    hold = {"mode": "hold-before-send", "generation": 1, "release": False, "methods": []}
    passing = {"mode": "pass", "generation": 2, "release": False, "methods": []}
    readonly = {"mode": mode, "generation": 3, "release": False, "methods": ["eth_getLogs"]}
    controls = [hold, hold, passing, passing] if during_identity else [hold, hold, readonly]
    calls = []

    class State:
        def __init__(self):
            self.lock = threading.RLock()
            self.send_locks = {}; self.accepted = {}
            self.arm = None; self.held_fingerprint = None; self.dropped_arm = None
            self.args = types.SimpleNamespace(max_hold_seconds=5)
            self.metrics = {"requests": 0, "method_counts": {}, "send_requests": 0, "upstream_send_attempts": 0,
                            "deduplicated_send_responses": 0, "held_requests": 0, "abandoned_held_requests": 0, "blocked_write_requests": 0}
            self.index = 0; self.events = []
        def configuration(self):
            control = controls[min(self.index, len(controls)-1)]; self.index += 1
            return control
        def write_metrics(self): pass
        def event(self, name, *args, **kwargs): self.events.append(name)
        def identity(self):
            calls.append("identity_read")
            if during_identity: controls.append(readonly)
        def upstream(self, body):
            calls.append(json.loads(body)["method"])
            raise AssertionError("read-only mode must not forward a send")

    state = State()
    handler = object.__new__(module.Handler); handler.server = types.SimpleNamespace(state=state)
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "eth_sendRawTransaction", "params": ["0x01"]}).encode()
    handler.headers = {"Content-Length": str(len(body))}; handler.rfile = io.BytesIO(body)
    handler.disconnected = lambda: False
    responses = []; handler.respond = lambda status, data, headers=None: responses.append(status)
    handler.do_POST()
    return state, calls, responses


class ReadOnlySendPolicy(unittest.TestCase):
    def test_held_send_is_rejected_when_rate_limit_is_armed(self):
        state, calls, responses = held_then_readonly("rate-limit")
        self.assertEqual(calls, []); self.assertEqual(responses, [503])
        self.assertEqual(state.metrics["upstream_send_attempts"], 0)
        self.assertEqual(state.metrics["blocked_write_requests"], 1)
        self.assertEqual(state.metrics["held_requests"], 0)

    def test_held_send_is_rejected_when_read_timeout_is_armed(self):
        state, calls, responses = held_then_readonly("read-timeout")
        self.assertEqual(calls, []); self.assertEqual(responses, [503])
        self.assertEqual(state.metrics["upstream_send_attempts"], 0)
        self.assertEqual(state.metrics["blocked_write_requests"], 1)
        self.assertEqual(state.metrics["held_requests"], 0)

    def test_control_is_checked_again_after_slow_identity_reads(self):
        for mode in ("rate-limit", "read-timeout"):
            with self.subTest(mode=mode):
                state, calls, responses = held_then_readonly(mode, during_identity=True)
                self.assertEqual(calls, ["identity_read"]); self.assertEqual(responses, [503])
                self.assertEqual(state.metrics["upstream_send_attempts"], 0)
                self.assertEqual(state.metrics["blocked_write_requests"], 1)


if __name__ == "__main__": unittest.main(verbosity=2)
