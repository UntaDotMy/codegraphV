"""Run one full index of a source tree and report wall time, CPU-seconds and peak memory.

Shared by efficiency_gate.py and memory_budget_gate.py. Peak memory is the process's private
commit (what it asked the OS to back with memory or page file), the number that matters for
"does this fit"; peak working set is reported too.

    python scripts/bench/proc_measure.py EXE SOURCE_DIR [--env K=V ...] [--db-out FILE]
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SKIP = shutil.ignore_patterns('.git', '.codegraph', '.codegraphV', 'node_modules')


def _windows_usage(proc):
    import ctypes
    import ctypes.wintypes as w

    class Counters(ctypes.Structure):
        _fields_ = [('cb', w.DWORD), ('PageFaultCount', w.DWORD), ('PeakWorkingSetSize', ctypes.c_size_t),
                    ('WorkingSetSize', ctypes.c_size_t), ('QuotaPeakPagedPoolUsage', ctypes.c_size_t),
                    ('QuotaPagedPoolUsage', ctypes.c_size_t), ('QuotaPeakNonPagedPoolUsage', ctypes.c_size_t),
                    ('QuotaNonPagedPoolUsage', ctypes.c_size_t), ('PagefileUsage', ctypes.c_size_t),
                    ('PeakPagefileUsage', ctypes.c_size_t)]

    created, exited, kernel, user = (w.FILETIME() for _ in range(4))
    handle = int(proc._handle)
    ctypes.windll.kernel32.GetProcessTimes(handle, ctypes.byref(created), ctypes.byref(exited),
                                           ctypes.byref(kernel), ctypes.byref(user))
    seconds = lambda f: ((f.dwHighDateTime << 32) | f.dwLowDateTime) / 1e7
    counters = Counters()
    counters.cb = ctypes.sizeof(Counters)
    ctypes.windll.psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb)
    return {
        'user_s': seconds(user), 'kernel_s': seconds(kernel),
        'peak_commit_mb': counters.PeakPagefileUsage / 1048576,
        'peak_ws_mb': counters.PeakWorkingSetSize / 1048576,
    }


def _posix_usage(_proc):
    import resource
    r = resource.getrusage(resource.RUSAGE_CHILDREN)
    peak = r.ru_maxrss / (1048576 if sys.platform == 'darwin' else 1024)
    return {'user_s': r.ru_utime, 'kernel_s': r.ru_stime, 'peak_commit_mb': peak, 'peak_ws_mb': peak}


def measure(exe, source, env_extra=None, db_out=None):
    """Index a copy of `source` with `exe init .`; returns the numbers, optionally saves the db."""
    work = Path(tempfile.mkdtemp(prefix='cg-proc-'))
    try:
        repo = work / 'repo'
        shutil.copytree(source, repo, ignore=SKIP)
        env = dict(os.environ, CODEGRAPHV_NO_UPDATE_CHECK='1', CODEGRAPH_NO_UPDATE_CHECK='1')
        env.update(env_extra or {})
        start = time.perf_counter()
        proc = subprocess.Popen([str(Path(exe).resolve()), 'init', '.'], cwd=repo, env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        err = proc.stderr.read()
        proc.wait()
        wall = time.perf_counter() - start
        if proc.returncode != 0:
            raise SystemExit(f'{exe} init failed ({proc.returncode}): {err[-2000:]}')
        usage = _windows_usage(proc) if os.name == 'nt' else _posix_usage(proc)
        usage['wall_s'] = wall
        usage['cpu_s'] = usage['user_s'] + usage['kernel_s']
        if db_out:
            for folder in ('.codegraphV', '.codegraph'):
                db = repo / folder / 'codegraph.db'
                if db.exists():
                    shutil.copyfile(db, db_out)
        return usage
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('exe')
    ap.add_argument('source')
    ap.add_argument('--env', action='append', default=[])
    ap.add_argument('--db-out')
    a = ap.parse_args()
    env = dict(kv.split('=', 1) for kv in a.env)
    print(json.dumps({k: round(v, 2) for k, v in measure(a.exe, a.source, env, a.db_out).items()}))


if __name__ == '__main__':
    main()
