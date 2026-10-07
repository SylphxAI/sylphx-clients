#!/usr/bin/env python3
"""Protocol and failure-path tests for the release smoke client."""

import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import unittest

spec = importlib.util.spec_from_file_location(
    "smoke_mcp", Path(__file__).with_name("smoke-mcp.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class SmokeTests(unittest.TestCase):
    def run_server(self, source, timeout=5):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            module.smoke([sys.executable, "-I", "-c", source], timeout=timeout)
        return output.getvalue()

    def test_full_handshake_waits_for_each_response_and_ignores_notifications(self):
        output = self.run_server('''
import json, sys
request = json.loads(sys.stdin.readline())
assert request["params"]["capabilities"] == {}
assert request["params"]["clientInfo"]["name"] == "release-smoke"
print(json.dumps({"jsonrpc":"2.0","method":"notifications/message"}), flush=True)
print(json.dumps({"jsonrpc":"2.0","id":1,"result":{
    "serverInfo":{"name":"test"},"protocolVersion":"2025-06-18"}}), flush=True)
assert json.loads(sys.stdin.readline())["method"] == "notifications/initialized"
assert json.loads(sys.stdin.readline())["method"] == "tools/list"
print(json.dumps({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"test"}]}}), flush=True)
assert sys.stdin.read() == ""
''')
        self.assertIn("initialize, initialized, tools/list", output)

    def test_protocol_error_is_not_a_success(self):
        with self.assertRaisesRegex(RuntimeError, "request 1 failed"):
            self.run_server('''
import json, sys
sys.stdin.readline()
print(json.dumps({"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad params"}}), flush=True)
sys.stdin.read()
''')

    def test_early_eof_is_not_a_success(self):
        with self.assertRaisesRegex(RuntimeError, "closed stdout before response"):
            self.run_server("import sys; sys.stdin.readline()")

    def test_silent_server_is_bounded(self):
        with self.assertRaises(TimeoutError):
            self.run_server("import sys; sys.stdin.read()", timeout=0.2)

    def test_nonzero_exit_after_response_is_not_a_success(self):
        with self.assertRaisesRegex(RuntimeError, "exited 3"):
            self.run_server('''
import json, sys
sys.stdin.readline()
print(json.dumps({"jsonrpc":"2.0","id":1,"result":{
    "serverInfo":{"name":"test"},"protocolVersion":"2025-06-18"}}), flush=True)
sys.stdin.readline()
sys.stdin.readline()
print(json.dumps({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"test"}]}}), flush=True)
sys.stdin.read()
sys.exit(3)
''')


if __name__ == "__main__":
    unittest.main()
