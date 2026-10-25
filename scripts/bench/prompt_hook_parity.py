"""Run the TS prompt-hook (dist/) and the Rust one on the same prompts and compare what they print.

    python scripts/bench/prompt_hook_parity.py [--show]

A small fixture project is indexed twice, once per engine, and each hook reads its own index.
Every output must be identical, byte for byte, except one part: a HIGH-tier injection's body is
the engine's own codegraph_explore output, and the Rust explore renderer is deliberately its own
design (DEV-19). For those the wrapper (note line, nudges, closing tag) must match exactly and
the Rust body must be non-empty.
Exits 0 when every case matches.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bench  # noqa: E402
sys.path.insert(0, str(HERE.parent))
from check_names import rename as in_codegraphv_names  # noqa: E402

FILES = {
    'src/auth.ts': 'export function parseToken(raw: string): string {\n  return raw.trim();\n}\n\n'
                   'export function login(user: string): string {\n  return parseToken(user);\n}\n',
    'src/order-state-machine.ts': 'export class OrderStateMachine {\n  advance(): void {}\n}\n',
    'src/users.ts': 'export function getUserId(name: string): number {\n  return name.length;\n}\n',
}

# (label, prompt, which project: 'single' | 'mono' | 'none')
CASES = [
    ('keyword', 'how does login work end to end?', 'single'),
    ('verified-token', 'parseToken returns what exactly', 'single'),
    ('unverified-token', 'I love JavaScript and GitHub', 'single'),
    ('prose-segment', 'the order state machine seems off', 'single'),
    ('non-structural', 'fix this typo please', 'single'),
    ('task-notification', '<task-notification>agent done</task-notification>', 'single'),
    ('reminder-wrapped-yes', '<system-reminder>linked sessions: 2</system-reminder>\nyes', 'single'),
    ('reminder-wrapped-keyword', '<system-reminder>status</system-reminder> how does login work', 'single'),
    ('cross-session', '<cross-session-message from="x">hi</cross-session-message>', 'single'),
    ('no-index', 'how does login work', 'none'),
    ('monorepo-nudge', 'how does login work', 'mono'),
    ('monorepo-named', 'how does login work in packages/api', 'mono'),
]


def write_project(root: Path):
    for rel, text in FILES.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding='utf8', newline='\n')


def index(cmd: list, root: Path, env: dict):
    r = subprocess.run(cmd + ['init', '--yes', '.'], cwd=root, env=env, capture_output=True, text=True,
                       encoding='utf8', errors='replace')
    if r.returncode != 0:
        raise SystemExit(f'index failed in {root}: {r.stdout}\n{r.stderr}')


def layout(base: Path, engine_cmd: list, env: dict) -> dict:
    single = base / 'single'
    write_project(single)
    index(engine_cmd, single, env)
    mono = base / 'mono'
    for pkg in ('packages/api', 'packages/web'):
        write_project(mono / pkg)
        index(engine_cmd, mono / pkg, env)
    (mono / 'package.json').write_text('{"name":"mono","private":true,"workspaces":["packages/*"]}\n', encoding='utf8')
    none = base / 'none'
    none.mkdir()
    return {'single': single, 'mono': mono, 'none': none}


def hook(cmd: list, cwd: Path, prompt: str, env: dict) -> str:
    payload = json.dumps({'prompt': prompt, 'cwd': str(cwd)})
    r = subprocess.run(cmd + ['prompt-hook'], input=payload, cwd=cwd, env=env, capture_output=True, text=True,
                       encoding='utf8')
    if r.returncode != 0:
        return f'<exit {r.returncode}>{r.stdout}'
    return r.stdout


HIGH = re.compile(r'^(<codegraphV_context note="Structural context[^\n]*\n)(.*?)((?:\nOther indexed projects[\s\S]*)?\n</codegraphV_context>\n)$', re.S)


def compare(ts: str, rust: str, ts_root: Path, rust_root: Path) -> tuple[bool, str]:
    ts = in_codegraphv_names(ts.replace(str(ts_root), '<ROOT>'))  # the original's text, in codegraphV's names
    rust = rust.replace(str(rust_root), '<ROOT>')
    m_ts, m_rust = HIGH.match(ts), HIGH.match(rust)
    if m_ts and m_rust:
        same = m_ts.group(1) == m_rust.group(1) and m_ts.group(3) == m_rust.group(3) and m_rust.group(2).strip() != ''
        return same, 'high (wrapper compared, body = engine explore)'
    if bool(m_ts) != bool(m_rust):
        return False, 'tier differs'
    return ts == rust, 'exact'


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--show', action='store_true')
    a = ap.parse_args()
    env = dict(os.environ, CODEGRAPH_TELEMETRY='0', CODEGRAPH_NO_UPDATE_CHECK='1', CODEGRAPH_DAEMON='0',
               CODEGRAPHV_NO_UPDATE_CHECK='1', CODEGRAPHV_DAEMON='0')
    env.pop('CODEGRAPH_NO_PROMPT_HOOK', None)
    env.pop('CODEGRAPHV_NO_PROMPT_HOOK', None)
    work = Path(tempfile.mkdtemp(prefix='cg-hook-'))
    try:
        ts_cmd, rust_cmd = bench.ts_cmd(), [str(bench.rust_bin())]
        (work / 'ts').mkdir()
        (work / 'rust').mkdir()
        ts_dirs = layout(work / 'ts', ts_cmd, env)
        rust_dirs = layout(work / 'rust', rust_cmd, env)
        failed = 0
        for label, prompt, where in CASES:
            ts_out = hook(ts_cmd, ts_dirs[where], prompt, env)
            rust_out = hook(rust_cmd, rust_dirs[where], prompt, env)
            ok, how = compare(ts_out, rust_out, work / 'ts', work / 'rust')
            failed += not ok
            kind = 'empty' if not ts_out else how
            print(f"{'ok  ' if ok else 'FAIL'} {label:26} {kind}")
            if a.show or not ok:
                print(f'--- ts\n{ts_out}--- rust\n{rust_out}---')
        print(f'{len(CASES) - failed}/{len(CASES)} prompt-hook cases match')
        return 1 if failed else 0
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
