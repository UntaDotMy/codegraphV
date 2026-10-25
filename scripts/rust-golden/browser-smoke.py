"""Run the packaged native viewer in a real Chromium browser, offline.

The fixture and isolated browser profile are owned by target/native-smoke.
No agent configuration or user browser profile is touched.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
WORK = ROOT / "target" / "native-smoke"
BUNDLE = ROOT / "target" / "native-bundle"
PROJECT = WORK / "project"
PROJECT.mkdir(parents=True, exist_ok=True)
(PROJECT / "package.json").write_text(json.dumps({"name": "receipt-service", "dependencies": {"express": "*"}}))
(PROJECT / "mail.ts").write_text("export function sendReceipt(email: string) { return `Receipt for ${email}`; }\n", encoding="utf-8")
(PROJECT / "users.ts").write_text("import { sendReceipt } from './mail';\nexport function saveUser(email: string) { return sendReceipt(email); }\nexport function createUser(email: string) { return saveUser(email); }\n", encoding="utf-8")
(PROJECT / "main.ts").write_text("import express from 'express';\nimport { createUser } from './users';\nconst app = express();\napp.post('/users', createUser);\n", encoding="utf-8")
(PROJECT / "users.test.ts").write_text("import { createUser } from './users';\nexport function testCreateUser() { return createUser('reader@example.test'); }\n", encoding="utf-8")
binary = BUNDLE / ("codegraph.exe" if os.name == "nt" else "codegraph")
environment = dict(os.environ, CODEGRAPH_UI="1", CODEGRAPH_TELEMETRY="0", DO_NOT_TRACK="1")
subprocess.run([str(binary), "init", "--yes"], cwd=PROJECT, env=environment, check=True, capture_output=True)
chrome = next((Path(p) for p in [os.environ.get("CODEGRAPH_TEST_BROWSER", ""), r"C:\Program Files\Google\Chrome\Application\chrome.exe", r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe", "/usr/bin/chromium", "/usr/bin/google-chrome"] if p and Path(p).is_file()), None)
if not chrome:
    raise SystemExit("Set CODEGRAPH_TEST_BROWSER to a Chromium executable")
server = subprocess.Popen([str(binary), "ui", "--no-open", "--port", "0"], cwd=PROJECT, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
try:
    line = server.stdout.readline()
    match = re.search(r"http://127\.0\.0\.1:\d+", line)
    if not match:
        raise RuntimeError(f"Viewer did not start: {line} {server.stderr.read()}")
    url = match.group()
    def api(path):
        with urllib.request.urlopen(url + path, timeout=10) as response:
            return json.load(response)
    found = api("/api/search?q=createUser")
    symbol = next(n for n in found["results"]["items"] if n["name"] == "createUser")
    from urllib.parse import quote
    checks = [("home", "#home", "Start reading"), ("symbol", "#symbol?id=" + quote(symbol["id"], safe=""), "Calls and uses"), ("file", "#file?path=users.ts", "Symbols"), ("flow", "#flow?symbols=createUser%20saveUser%20sendReceipt", "sendReceipt"), ("map", "#map", "Module map"), ("routes", "#routes", "/users"), ("trails", "#trails", "Saved trails")]
    flags = [str(chrome), "--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--disable-extensions", "--virtual-time-budget=12000", "--window-size=1440,1000"]
    for name, fragment, expected in checks:
        result = subprocess.run(flags + ["--user-data-dir=" + str(WORK / ("browser-" + name)), "--dump-dom", url + "/" + fragment], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=45)
        (WORK / (name + ".html")).write_text(result.stdout, encoding="utf-8")
        (WORK / (name + "-browser.log")).write_text(result.stderr, encoding="utf-8")
        if expected not in result.stdout or "Could not open this view" in result.stdout or "Could not open viewer:" in result.stdout:
            raise AssertionError(f"{name}: expected {expected!r}; inspect {WORK / (name + '.html')}")
        print(f"PASS {name}", flush=True)
    subprocess.run(flags + ["--user-data-dir=" + str(WORK / "browser-screenshot"), "--screenshot=" + str(WORK / "viewer.png"), url + "/#symbol?id=" + quote(symbol["id"], safe="")], capture_output=True, timeout=45, check=True)
    print(f"Screenshot: {WORK / 'viewer.png'}", flush=True)
finally:
    server.terminate()
    server.wait(timeout=10)
