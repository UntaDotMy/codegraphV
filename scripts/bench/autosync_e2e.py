"""End-to-end check that the Rust MCP server keeps the graph in sync with file edits.

    python scripts/bench/autosync_e2e.py [--mode daemon|direct|both]

For each mode it makes a small TypeScript project, indexes it with `codegraph init`, starts
`codegraph serve --mcp` (daemon mode is the default launch; direct sets CODEGRAPH_DAEMON=0) and,
talking JSON-RPC over stdio like an agent, checks that each change becomes visible to
codegraph_search without any manual sync:

  offline   a function added before the server starts is found on the first query
  add       a function appended to an existing file is found
  delete    that function, removed again, is gone
  new-file  a function in a newly created file is found
  rename    a renamed file's function is reported under the new path only

Each step waits up to --timeout seconds. Exits 0 when every step passes in every mode.
"""
import argparse
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bench  # noqa: E402


class Server:
    def __init__(self, exe: Path, project: Path, env: dict):
        self.proc = subprocess.Popen([str(exe), 'serve', '--mcp', '--path', str(project)], cwd=project, env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.lines: queue.Queue = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()
        self.next_id = 0
        self.request('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                    'clientInfo': {'name': 'autosync-e2e', 'version': '0'}})
        self.notify('notifications/initialized')

    def _pump(self):
        for raw in self.proc.stdout:
            self.lines.put(raw)

    def notify(self, method: str, params=None):
        msg = {'jsonrpc': '2.0', 'method': method}
        if params is not None:
            msg['params'] = params
        self.proc.stdin.write((json.dumps(msg) + '\n').encode())
        self.proc.stdin.flush()

    def request(self, method: str, params: dict, timeout: float = 60):
        self.next_id += 1
        rid = self.next_id
        self.proc.stdin.write((json.dumps({'jsonrpc': '2.0', 'id': rid, 'method': method, 'params': params}) + '\n').encode())
        self.proc.stdin.flush()
        end = time.time() + timeout
        while time.time() < end:
            try:
                msg = json.loads(self.lines.get(timeout=max(0.1, end - time.time())))
            except queue.Empty:
                break
            if msg.get('id') == rid:
                return msg
            if 'id' in msg and 'method' in msg:  # a server request (roots/list): answer empty
                reply = {'jsonrpc': '2.0', 'id': msg['id'], 'result': {'roots': []}}
                self.proc.stdin.write((json.dumps(reply) + '\n').encode())
                self.proc.stdin.flush()
        raise TimeoutError(f'no response to {method}')

    def search(self, name: str) -> str:
        msg = self.request('tools/call', {'name': 'codegraph_search', 'arguments': {'query': name}})
        return ''.join(c.get('text', '') for c in msg.get('result', {}).get('content', []))

    def found(self, name: str) -> bool:
        # A miss echoes the query ("No results found for ..."): match the result row.
        return f'**{name}**' in self.search(name)

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=15)
        except Exception:
            self.proc.kill()


def wait_for(check, timeout: float) -> float | None:
    start = time.time()
    while time.time() - start < timeout:
        if check():
            return time.time() - start
        time.sleep(0.25)
    return None


def write(path: Path, text: str):
    path.write_text(text, encoding='utf8', newline='\n')


def run_mode(exe: Path, mode: str, timeout: float) -> bool:
    project = Path(tempfile.mkdtemp(prefix='cg-autosync-'))
    env = dict(os.environ)
    if mode == 'direct':
        env['CODEGRAPH_DAEMON'] = '0'
    server = None
    ok = True

    def step(name: str, check) -> None:
        nonlocal ok
        took = wait_for(check, timeout)
        ok &= took is not None
        print(f"  {'ok  ' if took is not None else 'FAIL'} {name}" + (f' ({took:.1f}s)' if took is not None else f' (not within {timeout:.0f}s)'))

    try:
        write(project / 'main.ts', 'export function mainEntry(): number {\n  return 1;\n}\n')
        init = subprocess.run([str(exe), 'init', '.'], cwd=project, env=env, capture_output=True, text=True)
        if init.returncode != 0:
            print(init.stdout, init.stderr)
            return False
        # Edited while no server runs: the server's start-up catch-up must pick it up.
        write(project / 'main.ts', 'export function mainEntry(): number {\n  return 1;\n}\n\nexport function offlineAdded(): void {}\n')
        server = Server(exe, project, env)
        first = server.found('offlineAdded')  # one query, no polling: the catch-up gates it
        ok &= first
        print(f"  {'ok  ' if first else 'FAIL'} offline edit seen by the very first query")

        with open(project / 'main.ts', 'a', encoding='utf8', newline='\n') as f:
            f.write('\nexport function liveAdded(): string {\n  return "x";\n}\n')
        step('add: appended function found', lambda: server.found('liveAdded'))

        write(project / 'main.ts', 'export function mainEntry(): number {\n  return 1;\n}\n')
        step('delete: removed function gone', lambda: not server.found('liveAdded'))

        write(project / 'extra.ts', 'export class FreshWidget {\n  render(): void {}\n}\n')
        step('new-file: function in new file found', lambda: server.found('FreshWidget'))

        os.replace(project / 'extra.ts', project / 'moved.ts')

        def renamed():
            out = server.search('FreshWidget')
            return '**FreshWidget**' in out and 'moved.ts' in out and 'extra.ts' not in out
        step('rename: symbol reported under new path only', renamed)
    finally:
        if server:
            server.close()
        if mode == 'daemon':
            subprocess.run([str(exe), 'daemon', '--stop', '--path', str(project)], cwd=project,
                           capture_output=True, env=env)
        time.sleep(0.5)
        shutil.rmtree(project, ignore_errors=True)
    return ok


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--mode', choices=['daemon', 'direct', 'both'], default='both')
    ap.add_argument('--timeout', type=float, default=10)
    a = ap.parse_args()
    exe = bench.rust_bin()
    modes = ['daemon', 'direct'] if a.mode == 'both' else [a.mode]
    results = {}
    for mode in modes:
        print(f'== {mode}')
        results[mode] = run_mode(exe, mode, a.timeout)
    print(' '.join(f"{m}={'PASS' if r else 'FAIL'}" for m, r in results.items()))
    return 0 if all(results.values()) else 1


if __name__ == '__main__':
    sys.exit(main())
