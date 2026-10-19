#!/usr/bin/env python3
"""用本地 HTTP 运行器验证 Home 回归脚本，不连接或操作手机。"""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import threading
import types
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "runner_home_check", Path(__file__).with_name("runner-home-check.py")
)
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class HomeCheckTests(unittest.TestCase):
    """覆盖切换时的短暂错误、持续失败和禁止重放操作。"""

    def run_check(self, fault=None, persistent=False):
        """运行真实脚本入口，以本地响应代替手机并加速轮询时钟。"""
        state = {"bundle": CHECK.CALCULATOR, "pending": False, "requests": []}
        clock = types.SimpleNamespace(now=0.0)

        def sleep(seconds):
            """推进测试时钟，避免持续失败场景实际等待四秒。"""
            clock.now += seconds

        class Handler(BaseHTTPRequestHandler):
            """模拟脚本使用的启动、Home 和前台应用接口。"""

            def log_message(self, *args):
                """关闭本地服务的访问日志，保留测试结果。"""

            def respond(self, status, value):
                """发送与运行器一致的 runner JSON 包装。"""
                data = json.dumps({"value": value}).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_POST(self):
                """每个操作更新前台状态，并触发下一次查询的切换窗口。"""
                state["requests"].append(("POST", self.path))
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if fault == "action_error":
                    self.respond(500, {"error": "unknown error", "message": "launch failed"})
                    return
                if self.path == "/launch":
                    state["bundle"] = body["bundle"]
                else:
                    state["bundle"] = CHECK.SPRINGBOARD
                state["pending"] = True
                self.respond(200, None)

            def do_GET(self):
                """按场景返回短暂错误、错误应用或真实前台状态。"""
                state["requests"].append(("GET", self.path))
                if fault and (persistent or state["pending"]):
                    state["pending"] = False
                    if fault == "wrong_app":
                        self.respond(200, {"bundleId": "com.example.other", "pid": 99})
                    else:
                        self.respond(fault, {"error": "unknown error", "message": "no active application"})
                    return
                self.respond(200, {"bundleId": state["bundle"], "pid": 42})

        server = HTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        output = io.StringIO()
        try:
            with patch.object(sys, "argv", ["runner-home-check.py", "--base",
                                            f"http://127.0.0.1:{server.server_port}"]), \
                    patch.object(CHECK, "time", types.SimpleNamespace(
                        monotonic=lambda: clock.now, sleep=sleep)), \
                    patch.dict("os.environ", {"IPU_RUNNER_TOKEN": "0" * 64}), \
                    contextlib.redirect_stdout(output):
                result = CHECK.main()
        finally:
            server.shutdown()
            server.server_close()
            worker.join()
        return result, output.getvalue(), state["requests"], clock.now

    def test_all_home_routes_pass_when_foreground_is_ready(self):
        """前台状态正常时，三个 Home 入口及计算器进程检查全部通过。"""
        result, output, requests, elapsed = self.run_check()
        self.assertEqual(result, 0, output)
        for route in ("/home", "/wda/homescreen", "/wda/pressButton"):
            self.assertIn(f"PASS {route}:", output)
            self.assertEqual(requests.count(("POST", route)), 2)
        self.assertEqual(elapsed, 0)

    def test_transient_lookup_errors_recover_without_replaying_actions(self):
        """短暂 HTTP 404 或错误包装不应使成功的切换失败，也不重发操作。"""
        for status in (404, 200):
            with self.subTest(status=status):
                result, output, requests, elapsed = self.run_check(status)
                self.assertEqual(result, 0, output)
                self.assertEqual(output.count("PASS "), 3)
                self.assertEqual(len([r for r in requests if r[0] == "POST"]), 12)
                self.assertGreater(elapsed, 0)

    def test_wrong_app_is_polled_until_target_appears(self):
        """切换中短暂读到其他应用时，应继续等待目标应用。"""
        result, output, _, _ = self.run_check("wrong_app")
        self.assertEqual(result, 0, output)

    def test_persistent_lookup_errors_fail_at_deadline(self):
        """持续查询错误应在期限到达后失败，并保留错误信息。"""
        for status in (404, 200):
            with self.subTest(status=status):
                result, output, requests, elapsed = self.run_check(status, persistent=True)
                self.assertEqual(result, 1, output)
                self.assertEqual(output.count("FAIL "), 3)
                self.assertGreaterEqual(elapsed, 12)
                self.assertLessEqual(elapsed, 12.001)
                self.assertIn("404" if status == 404 else "no active application", output)
                self.assertEqual(len([r for r in requests if r[0] == "POST"]), 3)

    def test_persistent_wrong_app_is_not_reported_as_success(self):
        """目标应用一直不出现时，超时报告应包含最后读到的应用。"""
        result, output, _, elapsed = self.run_check("wrong_app", persistent=True)
        self.assertEqual(result, 1)
        self.assertIn("com.example.other", output)
        self.assertGreaterEqual(elapsed, 12)

    def test_authentication_errors_are_not_retried(self):
        """认证错误不能被当作切换窗口中的前台查询错误重试。"""
        result, output, requests, elapsed = self.run_check(401, persistent=True)
        self.assertEqual(result, 1)
        self.assertIn("401", output)
        self.assertEqual(len([r for r in requests if r[0] == "GET"]), 3)
        self.assertEqual(elapsed, 0)

    def test_failed_actions_are_not_replayed(self):
        """操作请求失败时每个入口仅尝试一次，不进入只读查询重试。"""
        result, output, requests, elapsed = self.run_check("action_error")
        self.assertEqual(result, 1)
        self.assertIn("500", output)
        self.assertEqual(requests, [("POST", "/launch")] * 3)
        self.assertEqual(elapsed, 0)


if __name__ == "__main__":
    unittest.main()
