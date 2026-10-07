#!/usr/bin/env python3
"""Smoke an MCP stdio server without closing stdin before its responses.

Usage: python3 -I scripts/smoke-mcp.py -- [runner ...] sylphx mcp
The optional runner supports the release's QEMU cross-compiled binary.
"""

import json
import os
import selectors
import subprocess
import sys
import time


def smoke(command, timeout=20):
    env = os.environ.copy()
    # No platform calls or credentials are needed for initialize/tools/list.
    env.pop("SYLPHX_API_KEY", None)
    with subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env
    ) as process, selectors.DefaultSelector() as ready:
        ready.register(process.stdout, selectors.EVENT_READ)
        deadline = time.monotonic() + timeout
        pending = bytearray()

        def send(message):
            process.stdin.write(json.dumps(message).encode() + b"\n")
            process.stdin.flush()

        def response(request_id):
            while True:
                while b"\n" in pending:
                    line, _, rest = pending.partition(b"\n")
                    pending[:] = rest
                    value = json.loads(line)
                    if value.get("id") != request_id:
                        continue  # Server notifications may precede a response.
                    if "error" in value:
                        raise RuntimeError(f"MCP request {request_id} failed: {value['error']}")
                    if "result" not in value:
                        raise RuntimeError(f"MCP request {request_id} has no result")
                    return value["result"]
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not ready.select(remaining):
                    raise TimeoutError(f"MCP response {request_id} did not arrive")
                chunk = os.read(process.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError(f"MCP server closed stdout before response {request_id}")
                pending.extend(chunk)
                if len(pending) > 4 * 1024 * 1024:
                    raise RuntimeError("MCP response exceeds smoke-test size bound")

        try:
            send({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "release-smoke", "version": "1"},
                },
            })
            initialized = response(1)
            if not initialized.get("serverInfo", {}).get("name"):
                raise RuntimeError("initialize did not return serverInfo.name")
            if initialized.get("protocolVersion") != "2025-06-18":
                raise RuntimeError("initialize did not negotiate the requested protocol")
            send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
            listed = response(2)
            tools = listed.get("tools")
            if not isinstance(tools, list) or not tools:
                raise RuntimeError("tools/list did not return a nonempty tools array")
            process.stdin.close()
            status = process.wait(timeout=max(0.1, deadline - time.monotonic()))
            if status != 0:
                raise RuntimeError(f"MCP server exited {status} after handshake")
            print("MCP smoke passed: initialize, initialized, tools/list")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    args = sys.argv[1:]
    if args and args[0] == "--":
        args = args[1:]
    if not args:
        sys.exit("usage: smoke-mcp.py -- [runner ...] sylphx mcp")
    smoke(args)
