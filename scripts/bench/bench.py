#!/usr/bin/env python3
"""Benchmark: Rust codegraph vs TS codegraph vs a ripgrep+Read baseline.

    python scripts/bench/bench.py [--quick] [--runs N] [--repos a,b] [--arms rust,ts,rg]
    python scripts/bench/bench.py --check-queries

For every repo in queries.json the harness copies the source twice (one copy
per engine, so the two indexes never share a .codegraph/), indexes both, then
runs every query through every arm and records per query:

  recall      share of the query's target definitions whose source line the
              output shows (the agent can stop reading); `located` also counts
              a path:line pointer without source
  wall_ms     wall clock of the arm's processes
  cpu_ms      user+kernel CPU of the whole process tree
  peak_rss_mb largest working set of any process in the tree
  out_tokens  chars/4 of everything the agent would read (same rule for all arms)

Ground truth is computed from source text only (definition patterns per
language, see find_definitions) — never from either engine's index.

Measurement on Windows runs each command in a Job Object: CPU and commit are
exact for the whole tree, the main process's peak working set is read from its
handle after exit, children's peaks are sampled. Elsewhere CPU comes from
getrusage(RUSAGE_CHILDREN) and peaks are sampled.

The rg arm models an agent without codegraph: `rg -n -w <identifier>` per query
identifier (output capped at 250 lines, as the Grep tool does), then a Read of
up to 2000 lines of each file whose grep hit looks like a declaration (at most
2 files per identifier, each file once). It is an upper bound on what a
grep+read agent sees for that token budget.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tarfile
import io
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

try:
    import psutil
except ImportError:  # pragma: no cover - reported at runtime
    psutil = None

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
ENV = {**os.environ, 'DO_NOT_TRACK': '1', 'CODEGRAPH_NO_UPDATE_CHECK': '1', 'CODEGRAPH_TELEMETRY': '0',
       'CODEGRAPHV_NO_UPDATE_CHECK': '1'}
# The TS engine indexes into .codegraph/, the Rust one (codegraphV) into .codegraphV/.
INDEX_DIRS = {'ts': '.codegraph', 'rust': '.codegraphV'}
SKIP_DIRS = {'.codegraph', '.codegraphV', '.git', 'node_modules', 'target', 'dist'}
GREP_HEAD_LIMIT = 250
READ_LINE_LIMIT = 2000
READ_FILES_PER_IDENT = 2

# --------------------------------------------------------------------------
# Ground truth: definition sites from source text
# --------------------------------------------------------------------------

LANG_BY_EXT = {
    '.rs': 'rust',
    '.ts': 'ts', '.tsx': 'ts', '.mts': 'ts', '.cts': 'ts',
    '.js': 'ts', '.jsx': 'ts', '.mjs': 'ts', '.cjs': 'ts',
    '.py': 'python',
}


def _def_patterns(lang: str, name: str) -> list[re.Pattern]:
    n = re.escape(name)
    if lang == 'rust':
        vis = r'(?:pub(?:\s*\([^)]*\))?\s+)?'
        return [
            re.compile(rf'^\s*{vis}(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+{n}\b'),
            re.compile(rf'^\s*{vis}(?:unsafe\s+|auto\s+)?(?:struct|enum|trait|union|type|mod|static|const)\s+(?:mut\s+)?{n}\b'),
            re.compile(rf'^\s*macro_rules!\s*{n}\b'),
        ]
    if lang == 'ts':
        mods = r'(?:(?:public|private|protected|static|readonly|async|abstract|override|declare|get|set)\s+)*'
        return [
            re.compile(rf'^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?'
                       rf'(?:function\s*\*?|class|interface|type|enum|const|let|var|namespace)\s+{n}\b'),
            # class member: `name(…) {` / `name<T>(…): R {` — not a call statement
            re.compile(rf'^\s*{mods}\*?{n}\s*(?:<[^>]*>)?\s*\([^;]*$'),
            # property holding a function: `name = (…) =>` / `name: async (…) =>`
            re.compile(rf'^\s*{mods}{n}\s*[:=]\s*(?:async\s+)?(?:\([^)]*\)|\w+)\s*(?::[^=]*)?=>'),
        ]
    if lang == 'python':
        return [re.compile(rf'^\s*(?:async\s+)?def\s+{n}\b'), re.compile(rf'^\s*class\s+{n}\b')]
    return []


_NOT_DECL = re.compile(r'^\s*(?:return|await|if|while|for|switch|catch|new|throw|else|yield|case)\b')


def iter_source_files(root: Path):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for f in filenames:
            p = Path(dirpath, f)
            if p.suffix in LANG_BY_EXT:
                yield p


def read_lines(path: Path) -> list[str]:
    try:
        return path.read_text(encoding='utf-8', errors='replace').splitlines()
    except OSError:
        return []


def find_definitions(root: Path, name: str, path_hint: str | None = None) -> list[tuple[str, int]]:
    """Definition sites of `name` as (posix relative path, 1-based line)."""
    sites = []
    for p in iter_source_files(root):
        rel = p.relative_to(root).as_posix()
        if path_hint and path_hint not in rel:
            continue
        pats = _def_patterns(LANG_BY_EXT[p.suffix], name)
        for i, line in enumerate(read_lines(p), 1):
            if name in line and not _NOT_DECL.match(line) and any(r.search(line) for r in pats):
                sites.append((rel, i))
    return sites


def parse_target(spec: str) -> tuple[str, str | None]:
    name, _, hint = spec.partition('@')
    return name, (hint or None)


# --------------------------------------------------------------------------
# What an output shows
# --------------------------------------------------------------------------

_HEADER = re.compile(r'^\*\*`?([^`*\s]+?)`?\*\*(?:\s|$)')
_NUMBERED = re.compile(r'^\s*(\d+)\t')
_POINTER = re.compile(r'([\w./@-]+\.\w+):(\d+)')
_RANGE = re.compile(r':(\d+)(?:[–-](\d+))?')


@dataclass
class Coverage:
    shown: set = field(default_factory=set)      # (path, line) with source text
    located: set = field(default_factory=set)    # (path, line) or ranges named by a pointer
    ranges: list = field(default_factory=list)   # (path, start, end)

    def is_shown(self, site):
        return site in self.shown

    def is_located(self, site):
        if site in self.shown or site in self.located:
            return True
        return any(p == site[0] and s <= site[1] <= e for p, s, e in self.ranges)


def parse_explore_output(text: str) -> Coverage:
    cov = Coverage()
    current = None
    in_fence = False
    for line in text.splitlines():
        if line.startswith('```'):
            in_fence = not in_fence
            continue
        if in_fence:
            m = _NUMBERED.match(line)
            if m and current:
                cov.shown.add((current, int(m.group(1))))
            continue
        h = _HEADER.match(line)
        if h and ('/' in h.group(1) or '.' in h.group(1)):
            current = h.group(1)
            continue
        for p, n in _POINTER.findall(line):
            cov.located.add((p, int(n)))
        if current and line.lstrip().startswith('- '):
            for s, e in _RANGE.findall(line):
                cov.ranges.append((current, int(s), int(e or s)))
    return cov


def score(cov: Coverage, targets: list[list[tuple[str, int]]]) -> tuple[float, float]:
    if not targets:
        return 0.0, 0.0
    shown = sum(any(cov.is_shown(s) for s in sites) for sites in targets)
    located = sum(any(cov.is_located(s) for s in sites) for sites in targets)
    return shown / len(targets), located / len(targets)


def tokens(text: str) -> int:
    return (len(text) + 3) // 4


# --------------------------------------------------------------------------
# Process measurement
# --------------------------------------------------------------------------

@dataclass
class Run:
    stdout: str
    code: int
    wall_ms: float
    cpu_ms: float
    peak_rss_mb: float
    peak_commit_mb: float | None
    procs: int


class _Sampler(threading.Thread):
    """Largest peak working set among the child's descendants."""

    def __init__(self, pid):
        super().__init__(daemon=True)
        self.pid, self.peak, self.stop = pid, 0, threading.Event()

    def run(self):
        if psutil is None:
            return
        try:
            root = psutil.Process(self.pid)
        except psutil.Error:
            return
        while not self.stop.is_set():
            try:
                procs = root.children(recursive=True)
                if os.name != 'nt':
                    procs.append(root)
            except psutil.Error:
                break
            for p in procs:
                try:
                    mi = p.memory_info()
                    self.peak = max(self.peak, getattr(mi, 'peak_wset', 0) or mi.rss)
                except psutil.Error:
                    pass
            self.stop.wait(0.01)


if os.name == 'nt':
    import ctypes
    from ctypes import wintypes

    _k32 = ctypes.WinDLL('kernel32', use_last_error=True)
    _psapi = ctypes.WinDLL('psapi', use_last_error=True)
    _ntdll = ctypes.WinDLL('ntdll')
    _k32.CreateJobObjectW.restype = wintypes.HANDLE

    class _IO(ctypes.Structure):
        _fields_ = [(n, ctypes.c_ulonglong) for n in (
            'ReadOperationCount', 'WriteOperationCount', 'OtherOperationCount',
            'ReadTransferCount', 'WriteTransferCount', 'OtherTransferCount')]

    class _BasicLimit(ctypes.Structure):
        _fields_ = [('PerProcessUserTimeLimit', ctypes.c_int64), ('PerJobUserTimeLimit', ctypes.c_int64),
                    ('LimitFlags', wintypes.DWORD), ('MinimumWorkingSetSize', ctypes.c_size_t),
                    ('MaximumWorkingSetSize', ctypes.c_size_t), ('ActiveProcessLimit', wintypes.DWORD),
                    ('Affinity', ctypes.c_size_t), ('PriorityClass', wintypes.DWORD),
                    ('SchedulingClass', wintypes.DWORD)]

    class _ExtLimit(ctypes.Structure):
        _fields_ = [('Basic', _BasicLimit), ('Io', _IO), ('ProcessMemoryLimit', ctypes.c_size_t),
                    ('JobMemoryLimit', ctypes.c_size_t), ('PeakProcessMemoryUsed', ctypes.c_size_t),
                    ('PeakJobMemoryUsed', ctypes.c_size_t)]

    class _Accounting(ctypes.Structure):
        _fields_ = [('TotalUserTime', ctypes.c_int64), ('TotalKernelTime', ctypes.c_int64),
                    ('ThisPeriodTotalUserTime', ctypes.c_int64), ('ThisPeriodTotalKernelTime', ctypes.c_int64),
                    ('TotalPageFaultCount', wintypes.DWORD), ('TotalProcesses', wintypes.DWORD),
                    ('ActiveProcesses', wintypes.DWORD), ('TotalTerminatedProcesses', wintypes.DWORD)]

    class _PMC(ctypes.Structure):
        _fields_ = [('cb', wintypes.DWORD), ('PageFaultCount', wintypes.DWORD),
                    ('PeakWorkingSetSize', ctypes.c_size_t), ('WorkingSetSize', ctypes.c_size_t),
                    ('QuotaPeakPagedPoolUsage', ctypes.c_size_t), ('QuotaPagedPoolUsage', ctypes.c_size_t),
                    ('QuotaPeakNonPagedPoolUsage', ctypes.c_size_t), ('QuotaNonPagedPoolUsage', ctypes.c_size_t),
                    ('PagefileUsage', ctypes.c_size_t), ('PeakPagefileUsage', ctypes.c_size_t)]

    def measure(cmd, cwd, timeout=1800) -> Run:
        job = _k32.CreateJobObjectW(None, None)
        if not job:
            raise OSError(ctypes.get_last_error(), 'CreateJobObjectW')
        try:
            lim = _ExtLimit()
            lim.Basic.LimitFlags = 0x2000  # KILL_ON_JOB_CLOSE: nothing outlives the run
            _k32.SetInformationJobObject(job, 9, ctypes.byref(lim), ctypes.sizeof(lim))
            t0 = time.perf_counter()
            proc = subprocess.Popen(cmd, cwd=cwd, env=ENV, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                    stdin=subprocess.DEVNULL, creationflags=0x4)  # CREATE_SUSPENDED
            handle = wintypes.HANDLE(int(proc._handle))
            if not _k32.AssignProcessToJobObject(job, handle):
                proc.kill()
                raise OSError(ctypes.get_last_error(), 'AssignProcessToJobObject')
            _ntdll.NtResumeProcess(handle)
            sampler = _Sampler(proc.pid)
            sampler.start()
            out, _ = proc.communicate(timeout=timeout)
            wall = (time.perf_counter() - t0) * 1000
            sampler.stop.set()
            sampler.join()
            acct, ext, pmc = _Accounting(), _ExtLimit(), _PMC()
            pmc.cb = ctypes.sizeof(pmc)
            _k32.QueryInformationJobObject(job, 1, ctypes.byref(acct), ctypes.sizeof(acct), None)
            _k32.QueryInformationJobObject(job, 9, ctypes.byref(ext), ctypes.sizeof(ext), None)
            _psapi.GetProcessMemoryInfo(handle, ctypes.byref(pmc), pmc.cb)
            return Run(out.decode('utf-8', 'replace'), proc.returncode, wall,
                       (acct.TotalUserTime + acct.TotalKernelTime) / 10_000,
                       max(pmc.PeakWorkingSetSize, sampler.peak) / 2**20,
                       ext.PeakJobMemoryUsed / 2**20, acct.TotalProcesses)
        finally:
            _k32.CloseHandle(job)
else:
    import resource

    def measure(cmd, cwd, timeout=1800) -> Run:
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        t0 = time.perf_counter()
        proc = subprocess.Popen(cmd, cwd=cwd, env=ENV, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                stdin=subprocess.DEVNULL)
        sampler = _Sampler(proc.pid)
        sampler.start()
        out, _ = proc.communicate(timeout=timeout)
        wall = (time.perf_counter() - t0) * 1000
        sampler.stop.set()
        sampler.join()
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        cpu = (after.ru_utime - before.ru_utime + after.ru_stime - before.ru_stime) * 1000
        return Run(out.decode('utf-8', 'replace'), proc.returncode, wall, cpu, sampler.peak / 2**20, None, 1)


# --------------------------------------------------------------------------
# Arms
# --------------------------------------------------------------------------

def rust_bin() -> Path:
    # CODEGRAPH_RUST_BIN points the parity tools at another build (a debug binary for small fixtures).
    override = os.environ.get('CODEGRAPH_RUST_BIN')
    exe = Path(override) if override else ROOT / 'target' / 'release' / ('codegraphV.exe' if os.name == 'nt' else 'codegraphV')
    if not exe.exists():
        sys.exit(f'missing {exe}: run `cargo build --release -p codegraph-cli` first')
    return exe


def ts_cmd() -> list[str]:
    js = ROOT / 'dist' / 'bin' / 'codegraph.js'
    if not js.exists():
        sys.exit(f'missing {js}: run `npm run build` first')
    return ['node', str(js)]


ENGINES = {
    'rust': {'index': lambda: [str(rust_bin()), 'init', '.'], 'explore': lambda q: [str(rust_bin()), 'explore', q]},
    'ts': {'index': lambda: ts_cmd() + ['init', '-y', '.'], 'explore': lambda q: ts_cmd() + ['explore', q]},
}


def query_identifiers(q: str) -> list[str]:
    out = []
    for tok in re.split(r'[\s.:]+', q):
        if re.fullmatch(r'[A-Za-z_]\w{2,}', tok) and tok not in out:
            out.append(tok)
    return out


_DECLISH = re.compile(r'\b(?:fn|struct|enum|trait|type|class|interface|function|def|const|let|var|mod|impl|macro_rules!)\s')


def run_rg_arm(repo: Path, q: str) -> tuple[Run, Coverage]:
    """grep each identifier, then Read the files whose hits look like declarations."""
    texts, wall, cpu, peak, procs = [], 0.0, 0.0, 0.0, 0
    cov, opened = Coverage(), set()
    for ident in query_identifiers(q):
        r = measure(['rg', '-n', '-w', '--no-heading', '--color', 'never', '-g', '!.codegraph', '-g', '!.codegraphV', '--', ident, '.'], repo)
        lines = r.stdout.splitlines()[:GREP_HEAD_LIMIT]
        texts.append('\n'.join(lines))
        wall, cpu, peak, procs = wall + r.wall_ms, cpu + r.cpu_ms, max(peak, r.peak_rss_mb), procs + r.procs
        picked = {}  # path -> first declaration-looking hit line
        for ln in lines:
            path, _, rest = ln.partition(':')
            num, _, body = rest.partition(':')
            path = path.replace('\\', '/').removeprefix('./')
            if path not in picked and num.isdigit() and (
                    _DECLISH.search(body) or re.search(rf'\b{re.escape(ident)}\s*(?:<[^>]*>)?\s*\([^;]*$', body)):
                picked[path] = int(num)
                if len(picked) >= READ_FILES_PER_IDENT:
                    break
        t0 = time.perf_counter()
        for path, hit in picked.items():
            if path in opened:
                continue
            opened.add(path)
            src = read_lines(repo / path)
            start = 1 if len(src) <= READ_LINE_LIMIT else max(1, min(hit - READ_LINE_LIMIT // 2, len(src) - READ_LINE_LIMIT + 1))
            chunk = src[start - 1:start - 1 + READ_LINE_LIMIT]
            texts.append('\n'.join(f'{start + i}\t{l}' for i, l in enumerate(chunk)))
            cov.shown.update((path, start + i) for i in range(len(chunk)))
        wall += (time.perf_counter() - t0) * 1000
    return Run('\n'.join(texts), 0, wall, cpu, peak, None, procs), cov


# --------------------------------------------------------------------------
# Repos
# --------------------------------------------------------------------------

def resolve_source(spec: dict) -> Path | None:
    if 'cargo' in spec:
        reg = Path.home() / '.cargo' / 'registry' / 'src'
        for d in reg.glob(f'*/{spec["cargo"]}'):
            return d
        return None
    return None


def prepare_copy(spec: dict, dest: Path):
    """Fresh copy of the repo source at dest (no index)."""
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    if spec.get('git') == 'HEAD':
        data = subprocess.run(['git', 'archive', '--format=tar', spec.get('rev', 'HEAD')],
                              cwd=ROOT, capture_output=True, check=True).stdout
        with tarfile.open(fileobj=io.BytesIO(data)) as tar:
            tar.extractall(dest, filter='data')
        return
    src = resolve_source(spec)
    if src is None:
        raise SystemExit(f'cannot find source for {spec["name"]}: {spec}')
    shutil.copytree(src, dest, dirs_exist_ok=True, ignore=shutil.ignore_patterns(*SKIP_DIRS))


def index_counts(repo: Path, engine: str) -> dict:
    db = repo / INDEX_DIRS[engine] / 'codegraph.db'
    if not db.exists():
        return {}
    con = sqlite3.connect(f'file:{db}?mode=ro', uri=True)
    try:
        return {t: con.execute(f'SELECT count(*) FROM {t}').fetchone()[0] for t in ('files', 'nodes', 'edges')}
    finally:
        con.close()


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------

def load_config() -> dict:
    return json.loads((HERE / 'queries.json').read_text(encoding='utf-8'))


def ground_truth(repo: Path, query: dict) -> list[list[tuple[str, int]]]:
    out = []
    for spec in query['targets']:
        name, hint = parse_target(spec)
        sites = find_definitions(repo, name, hint)
        if not sites:
            raise SystemExit(f'{query["id"]}: target {spec!r} has no definition in source — fix queries.json')
        out.append(sites)
    return out


def median_run(runs: list[Run]) -> dict:
    return {
        'wall_ms': round(statistics.median(r.wall_ms for r in runs), 1),
        'cpu_ms': round(statistics.median(r.cpu_ms for r in runs), 1),
        'peak_rss_mb': round(max(r.peak_rss_mb for r in runs), 1),
        'peak_commit_mb': None if runs[0].peak_commit_mb is None else round(max(r.peak_commit_mb for r in runs), 1),
        'procs': runs[0].procs,
    }


def fmt_table(report: dict) -> str:
    rows = ['| repo | arm | queries | recall | located | wall ms (med) | cpu ms (med) | peak RSS MB | tokens (med) |',
            '|---|---|---|---|---|---|---|---|---|']
    for repo, data in report['repos'].items():
        for arm, qs in data['arms'].items():
            ok = [q for q in qs if 'error' not in q]
            if not ok:
                rows.append(f'| {repo} | {arm} | 0 | - | - | - | - | - | - |')
                continue
            med = lambda k: statistics.median(q[k] for q in ok)
            rows.append(f'| {repo} | {arm} | {len(ok)} | {statistics.mean(q["recall"] for q in ok):.2f} | '
                        f'{statistics.mean(q["located"] for q in ok):.2f} | {med("wall_ms"):.0f} | {med("cpu_ms"):.0f} | '
                        f'{max(q["peak_rss_mb"] for q in ok):.0f} | {med("out_tokens"):.0f} |')
    rows += ['', '| repo | engine | files | nodes | edges | index wall ms | index cpu ms | avg cores | peak RSS MB | peak commit MB |',
             '|---|---|---|---|---|---|---|---|---|---|']
    for repo, data in report['repos'].items():
        for eng, ix in data['index'].items():
            c = ix.get('counts', {})
            rows.append(f'| {repo} | {eng} | {c.get("files", "-")} | {c.get("nodes", "-")} | {c.get("edges", "-")} | '
                        f'{ix["wall_ms"]:.0f} | {ix["cpu_ms"]:.0f} | {ix["cpu_ms"] / max(ix["wall_ms"], 1):.2f} | '
                        f'{ix["peak_rss_mb"]:.0f} | {ix["peak_commit_mb"] or 0:.0f} |')
    return '\n'.join(rows)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--work', default=str(Path(os.environ.get('TEMP', '/tmp')) / 'cg-bench'))
    ap.add_argument('--out', default=None, help='report path (default <work>/bench-report.json)')
    ap.add_argument('--repos', default=None, help='comma list of repo names')
    ap.add_argument('--arms', default='rust,ts,rg')
    ap.add_argument('--runs', type=int, default=3)
    ap.add_argument('--quick', action='store_true', help='1 run, no warmup')
    ap.add_argument('--reuse-index', action='store_true', help='skip copy + index when an index exists')
    ap.add_argument('--check-queries', action='store_true', help='print ground truth per target and exit')
    a = ap.parse_args(argv)

    if psutil is None:
        sys.exit('pip install psutil')
    cfg = load_config()
    repos = [r for r in cfg['repos'] if not a.repos or r['name'] in a.repos.split(',')]
    arms = a.arms.split(',')
    runs, warmup = (1, 0) if a.quick else (a.runs, 1)
    work = Path(a.work)

    if a.check_queries:
        for spec in repos:
            src = work / spec['name'] / 'src'
            prepare_copy(spec, src)
            for q in spec['queries']:
                for t, sites in zip(q['targets'], ground_truth(src, q)):
                    print(f'{spec["name"]} {q["id"]} {t}: {len(sites)} site(s) {sites[:3]}')
        return 0

    report = {'machine': {'cpus': os.cpu_count(), 'platform': sys.platform},
              'settings': {'runs': runs, 'warmup': warmup, 'arms': arms}, 'repos': {}}
    for spec in repos:
        name = spec['name']
        print(f'== {name}', flush=True)
        copies = {eng: work / name / eng for eng in ENGINES if eng in arms}
        index = {}
        for eng, repo in copies.items():
            if a.reuse_index and (repo / INDEX_DIRS[eng]).exists():
                index[eng] = {'reused': True, 'wall_ms': 0, 'cpu_ms': 0, 'peak_rss_mb': 0, 'peak_commit_mb': 0,
                              'counts': index_counts(repo, eng)}
                continue
            prepare_copy(spec, repo)
            r = measure(ENGINES[eng]['index'](), repo)
            if r.code != 0:
                sys.exit(f'{eng} index failed on {name} (exit {r.code})')
            index[eng] = {**median_run([r]), 'counts': index_counts(repo, eng)}
            print(f'  index {eng}: {r.wall_ms:.0f} ms wall, {r.cpu_ms:.0f} ms cpu, {r.peak_rss_mb:.0f} MB', flush=True)
        gt_repo = next(iter(copies.values()), None)
        if gt_repo is None:
            gt_repo = work / name / 'src'
            prepare_copy(spec, gt_repo)
        results = {arm: [] for arm in arms}
        for q in spec['queries']:
            truth = ground_truth(gt_repo, q)
            for arm in arms:
                row = {'id': q['id'], 'query': q['q']}
                try:
                    if arm == 'rg':
                        repo = gt_repo
                        for _ in range(warmup):
                            run_rg_arm(repo, q['q'])
                        measured = [run_rg_arm(repo, q['q']) for _ in range(runs)]
                        text, cov = measured[0][0].stdout, measured[0][1]
                        row.update(median_run([m[0] for m in measured]))
                    else:
                        repo = copies[arm]
                        cmd = ENGINES[arm]['explore'](q['q'])
                        for _ in range(warmup):
                            measure(cmd, repo)
                        measured = [measure(cmd, repo) for _ in range(runs)]
                        if measured[0].code != 0:
                            raise RuntimeError(f'exit {measured[0].code}')
                        text = measured[0].stdout
                        cov = parse_explore_output(text)
                        row.update(median_run(measured))
                    row['recall'], row['located'] = score(cov, truth)
                    row['out_tokens'] = tokens(text)
                except Exception as e:  # report, don't hide
                    row['error'] = str(e)
                results[arm].append(row)
            print('  ' + q['id'] + ' ' + ' '.join(
                f'{arm}={r[-1].get("recall", "ERR")!s:.4}/{r[-1].get("out_tokens", "-")}t/{r[-1].get("wall_ms", "-")}ms'
                for arm, r in results.items()), flush=True)
        report['repos'][name] = {'index': index, 'arms': results}

    out = Path(a.out) if a.out else work / 'bench-report.json'
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=1), encoding='utf-8')
    table = fmt_table(report)
    out.with_suffix('.md').write_text(table + '\n', encoding='utf-8')
    print(table)
    print(f'\nreport: {out}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
