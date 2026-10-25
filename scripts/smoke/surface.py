"""Smoke-test the Rust binary's user surface on a fresh copy of a bench repo.

    python scripts/smoke/surface.py cli      # every CLI command exits 0
    python scripts/smoke/surface.py mcp      # MCP stdio session, all 8 tools success-shaped
    python scripts/smoke/surface.py install  # install / uninstall round-trip in a sandbox home

The repo is serde_json from the cargo registry (see scripts/bench/queries.json). Every run
works in a temporary directory and deletes it; install runs with HOME / USERPROFILE /
APPDATA / XDG_CONFIG_HOME pointed into that directory, so no real config is touched.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / 'bench'))
import bench  # noqa: E402

TOOLS = ['explore', 'search', 'callers', 'callees', 'impact', 'node', 'files', 'status']
ENV = dict(os.environ, DO_NOT_TRACK='1', CODEGRAPH_NO_UPDATE_CHECK='1', CODEGRAPH_TELEMETRY='0')


def copy_repo(work: Path) -> Path:
    spec = next(r for r in json.load(open(HERE.parent / 'bench' / 'queries.json', encoding='utf-8'))['repos']
                if r['name'] == 'serde_json')
    repo = work / 'serde_json'
    bench.prepare_copy(spec, repo)
    return repo


def run(args, cwd, env=ENV, stdin=None):
    p = subprocess.run([str(bench.rust_bin()), *args], cwd=cwd, env=env, input=stdin,
                       capture_output=True, text=True, encoding='utf-8', errors='replace', timeout=600)
    return p.returncode, p.stdout, p.stderr


def check(ok: bool, label: str, detail: str = '') -> int:
    print(('PASS ' if ok else 'FAIL ') + label + ('' if ok or not detail else f'\n    {detail[:400]}'))
    return 0 if ok else 1


def smoke_cli(work: Path) -> int:
    repo = copy_repo(work)
    commands = [
        ['init', '.'],
        ['status'],
        ['query', 'from_str'],
        ['files', '--format', 'flat'],
        ['explore', 'from_str Deserializer'],
        ['node', 'from_str'],
        ['node', '--file', 'src/de.rs', '--limit', '20'],
        ['callers', 'parse_whitespace'],
        ['callees', 'from_str'],
        ['impact', 'parse_whitespace'],
        ['affected', 'src/de.rs'],
        ['context', 'how', 'is', 'a', 'number', 'parsed'],
        ['sync'],
        ['index'],
        ['uninit', '--force'],
    ]
    failed = 0
    for args in commands:
        code, out, err = run(args, repo)
        failed += check(code == 0, 'codegraph ' + ' '.join(args), err or out)
    failed += check(not (repo / '.codegraph').exists(), 'uninit removed .codegraph')
    return failed


def smoke_mcp(work: Path) -> int:
    repo = copy_repo(work)
    code, _, err = run(['init', '.'], repo)
    if check(code == 0, 'init for mcp', err):
        return 1
    env = dict(ENV, CODEGRAPH_MCP_TOOLS=','.join(TOOLS))
    proc = subprocess.Popen([str(bench.rust_bin()), 'serve', '--mcp', '--path', str(repo), '--no-watch'],
                            cwd=repo, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True, encoding='utf-8')
    failed = 0
    next_id = 0

    def request(method, params):
        nonlocal next_id
        next_id += 1
        proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': next_id, 'method': method, 'params': params}) + '\n')
        proc.stdin.flush()
        while True:
            line = proc.stdout.readline()
            if not line:
                raise RuntimeError(f'server closed during {method}')
            msg = json.loads(line)
            if msg.get('id') == next_id and 'method' not in msg:
                return msg
            if 'method' in msg and 'id' in msg:  # a server request (roots/list): answer empty
                proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': msg['id'], 'result': {'roots': []}}) + '\n')
                proc.stdin.flush()

    try:
        init = request('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'smoke', 'version': '0'}})
        failed += check('result' in init, 'initialize', json.dumps(init))
        proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        proc.stdin.flush()
        listed = request('tools/list', {})
        names = {t['name'] for t in listed.get('result', {}).get('tools', [])}
        failed += check(names == {f'codegraph_{t}' for t in TOOLS}, 'tools/list has all 8 tools', str(sorted(names)))
        calls = {
            'explore': {'query': 'from_str Deserializer'},
            'search': {'query': 'from_str'},
            'callers': {'symbol': 'parse_whitespace'},
            'callees': {'symbol': 'from_str'},
            'impact': {'symbol': 'parse_whitespace'},
            'node': {'symbol': 'from_str'},
            'files': {},
            'status': {},
        }
        for tool, args in calls.items():
            res = request('tools/call', {'name': f'codegraph_{tool}', 'arguments': args})
            result = res.get('result', {})
            text = ''.join(c.get('text', '') for c in result.get('content', []))
            ok = 'error' not in res and not result.get('isError') and len(text) > 20
            failed += check(ok, f'tools/call codegraph_{tool} ({len(text)} chars)', json.dumps(res)[:400])
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
    return failed


def snapshot(root: Path) -> dict:
    return {str(p.relative_to(root)): p.read_bytes() for p in root.rglob('*') if p.is_file()}


def smoke_install(work: Path) -> int:
    home = work / 'home'
    project = work / 'project'
    for d in (home / 'AppData' / 'Roaming', home / 'AppData' / 'Local', home / '.config', project):
        d.mkdir(parents=True, exist_ok=True)
    # Pre-existing user config the installer must preserve and restore: ~/.claude.json laid out
    # the way Claude Code writes it, and a CRLF CLAUDE.md as Windows editors save it.
    (home / '.claude.json').write_bytes(
        json.dumps({'theme': 'dark', 'mcpServers': {'other': {'command': 'x'}}}, indent=2).encode() + b'\n')
    (home / '.claude').mkdir()
    (home / '.claude' / 'CLAUDE.md').write_bytes(b'# My notes\r\n\r\nKeep this.\r\n')
    env = dict(ENV, HOME=str(home), USERPROFILE=str(home), APPDATA=str(home / 'AppData' / 'Roaming'),
               LOCALAPPDATA=str(home / 'AppData' / 'Local'), XDG_CONFIG_HOME=str(home / '.config'))
    for key in ('CLAUDE_CONFIG_DIR', 'CODEX_HOME', 'GEMINI_HOME', 'OPENCODE_CONFIG'):
        env.pop(key, None)
    before = snapshot(home)
    failed = 0
    code, out, err = run(['install', '--target', 'claude', '--location', 'global', '--yes'], project, env)
    failed += check(code == 0, 'install --target claude', err or out)
    after = snapshot(home)
    claude = json.loads((home / '.claude.json').read_text(encoding='utf-8'))
    server = claude.get('mcpServers', {}).get('codegraph', {})
    failed += check('codegraph' in str(server.get('command', '')).lower() or 'codegraph' in json.dumps(server).lower(),
                    'claude config has a codegraph MCP server', json.dumps(claude)[:400])
    failed += check(claude.get('mcpServers', {}).get('other') == {'command': 'x'} and claude.get('theme') == 'dark',
                    'sibling config preserved')
    code, out, err = run(['install', '--target', 'claude', '--location', 'global', '--yes'], project, env)
    failed += check(code == 0 and snapshot(home) == after, 're-install leaves files unchanged', err or out)
    code, out, err = run(['uninstall', '--target', 'claude', '--location', 'global', '--yes'], project, env)
    failed += check(code == 0, 'uninstall --target claude', err or out)
    restored = snapshot(home)
    changed = sorted(k for k in set(before) | set(restored) if before.get(k) != restored.get(k))
    failed += check(not changed, 'uninstall restores the original files byte-for-byte', ', '.join(changed))
    return failed


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else ''
    fn = {'cli': smoke_cli, 'mcp': smoke_mcp, 'install': smoke_install}.get(mode)
    if fn is None:
        print(__doc__)
        return 2
    work = Path(tempfile.mkdtemp(prefix='cg-smoke-'))
    try:
        failed = fn(work)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print(f'{mode}: {"OK" if not failed else f"{failed} failed"}')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
