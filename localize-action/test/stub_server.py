#!/usr/bin/env python3
"""Stub of the Sylphx Localization catalog endpoints, for tests.

usage: stub_server.py PORT_FILE LOG_FILE [--mode pass|error|flaky]

pass:  export has one warning.   error: export has one error (passed=false).
flaky: the first request answers 503 with Retry-After: 0, then behaves as pass.
Every request is appended to LOG_FILE as one JSON line.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

port_file, log_file = sys.argv[1], sys.argv[2]
mode = sys.argv[sys.argv.index("--mode") + 1] if "--mode" in sys.argv else "pass"
KEY = "test-key"
state = {"syncs": 0, "flaked": False, "last_sync": None}
PENDING = [2, 1, 0]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, code, obj, headers=None):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def handle_any(self, method):
        n = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(n)) if n else None
        with open(log_file, "a") as f:
            f.write(json.dumps({"method": method, "path": self.path, "body": body}) + "\n")
        if self.headers.get("Authorization") != "Bearer " + KEY:
            return self.reply(401, {"title": "Unauthorized", "detail": "bad key", "code": "UNAUTHENTICATED"})
        if mode == "flaky" and not state["flaked"]:
            state["flaked"] = True
            return self.reply(503, {"title": "Unavailable", "detail": "try later", "code": "UNAVAILABLE"},
                              {"Retry-After": "0"})
        path = self.path.split("?")[0]
        if method == "GET" and path == "/v1/envs/e/catalogs/c":
            return self.reply(200, {"spec": {"source_locale": "en", "target_locales": ["fr", "zh-Hans"],
                                             "pseudo_locales": ["accented"]}})
        if method == "POST" and path == "/v1/envs/e/catalogs/c:sync":
            i = min(state["syncs"], len(PENDING) - 1)
            state["syncs"] += 1
            state["last_sync"] = body
            resp = {"pending": PENDING[i], "usage": {"translated_characters": "100", "memory_matches": 1}}
            if i == 0:
                resp.update(added=3, changed=1)
            if i == 1:
                resp.update(translated=4)
            if i == 2:
                resp.update(translated=2, pinned=1, adopted=5, obsoleted=1)
                resp["usage"]["translated_characters"] = "50"
            if PENDING[i] == 0:
                resp.pop("pending")  # absent means 0
            return self.reply(200, resp)
        if method == "POST" and path == "/v1/envs/e/catalogs/c:export":
            paths = sorted({s["path"] for s in (state["last_sync"] or {}).get("sources", [])})
            files = []
            for loc in ("fr", "zh-Hans", "en-XA"):
                for p in paths:
                    files.append({"path": p, "locale": loc,
                                  "content": json.dumps({"locale": loc, "from": p}, ensure_ascii=False) + "\n"})
            findings = [{"key": "Hello|x", "locale": "fr", "check": "length_ratio",
                         "severity": "warning", "message": "long | text"}]
            passed = True
            errors = 0
            if mode == "error":
                findings.append({"key": "Bet", "locale": "zh-Hans", "check": "forbidden_term",
                                 "severity": "error", "message": "forbidden term"})
                passed, errors = False, 1
            report = {"warnings": 1, "findings": findings}
            if passed:
                report["passed"] = True
            else:
                report["errors"] = errors
            return self.reply(200, {"files": files, "report": report,
                                    "glyph_sets": [{"locale": "zh-Hans", "characters": "你好"}]})
        self.reply(404, {"title": "Not Found", "detail": path, "code": "NOT_FOUND"})

    def do_GET(self):
        self.handle_any("GET")

    def do_POST(self):
        self.handle_any("POST")


srv = HTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w") as f:
    f.write(str(srv.server_address[1]))
srv.serve_forever()
