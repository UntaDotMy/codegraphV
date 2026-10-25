"""Time a full index with two binaries on the same source and require a speedup.

    python scripts/bench/index_speed.py OLD_EXE NEW_EXE SOURCE_DIR [--runs 3] [--max-ratio 0.75]

Each run indexes a fresh copy of SOURCE_DIR (`init .`, the index folder removed first; the old
binary may be the original `codegraph` or codegraphV, so both `.codegraph` and `.codegraphV`
are removed). Prints every run and the medians. Exits 0 when median(new) <= max-ratio *
median(old), so 0.75 means "at least 25% faster".
"""
import argparse
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SKIP = shutil.ignore_patterns('.git', '.codegraph', '.codegraphV', 'node_modules')


def run_once(exe: str, source: Path) -> float:
    work = Path(tempfile.mkdtemp(prefix='cg-speed-'))
    try:
        repo = work / 'repo'
        shutil.copytree(source, repo, ignore=SKIP)
        env = dict(os.environ, CODEGRAPH_NO_UPDATE_CHECK='1', CODEGRAPHV_NO_UPDATE_CHECK='1')
        start = time.perf_counter()
        done = subprocess.run([exe, 'init', '.'], cwd=repo, env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.PIPE, text=True)
        wall = time.perf_counter() - start
        if done.returncode != 0:
            raise SystemExit(f'{exe} init failed ({done.returncode}): {done.stderr[-2000:]}')
        return wall
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('old')
    ap.add_argument('new')
    ap.add_argument('source')
    ap.add_argument('--runs', type=int, default=3)
    ap.add_argument('--max-ratio', type=float, default=0.75)
    a = ap.parse_args()
    source = Path(a.source)
    times = {'old': [], 'new': []}
    # Interleave so a machine slowing down over time affects both alike.
    for i in range(a.runs):
        # Absolute: each run starts inside a temporary copy of the source.
        for name, exe in (('old', str(Path(a.old).resolve())), ('new', str(Path(a.new).resolve()))):
            wall = run_once(exe, source)
            times[name].append(wall)
            print(f'run {i + 1} {name}: {wall:.1f} s', flush=True)
    old, new = statistics.median(times['old']), statistics.median(times['new'])
    ratio = new / old
    print(f'median old {old:.1f} s, new {new:.1f} s, new/old {ratio:.2f} ({(1 - ratio) * 100:.0f}% faster); '
          f'required <= {a.max_ratio:.2f}')
    return 0 if ratio <= a.max_ratio else 1


if __name__ == '__main__':
    sys.exit(main())
