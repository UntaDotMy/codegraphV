"""Gate: the new binary builds the SAME graph with no more wall time and clearly less CPU/memory.

    python scripts/bench/efficiency_gate.py OLD_EXE NEW_EXE SOURCE_DIR [--runs 3]
        [--max-wall 1.0] [--max-cpu 0.75] [--max-commit 0.70] [--graph-also DIR ...] [--graph-only]
        [--old-env K=V ...] [--new-env K=V ...]

Indexes SOURCE_DIR with both binaries (interleaved, `--runs` times each), requires the two
indexes to be identical (nodes, edges and unresolved refs, via compare_index.py), then compares
the medians: new/old wall <= --max-wall, CPU-seconds <= --max-cpu, peak commit <= --max-commit.
`--graph-also DIR` repeats the identity check on more trees; `--graph-only` skips the ratios.
`--old-env` / `--new-env` set environment variables for one binary only (e.g. `--new-env
CODEGRAPHV_MAX_FILE_MB=1` to compare against a build that skips sources over 1 MB).
Exit 0 when every check passes.
"""
import argparse
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from proc_measure import measure  # noqa: E402


def identical(old_db, new_db):
    done = subprocess.run([sys.executable, str(HERE / 'compare_index.py'), str(old_db), str(new_db)],
                          capture_output=True, text=True)
    print(done.stdout.strip()[-1500:])
    return done.returncode == 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('old')
    ap.add_argument('new')
    ap.add_argument('source')
    ap.add_argument('--runs', type=int, default=3)
    ap.add_argument('--max-wall', type=float, default=1.0)
    ap.add_argument('--max-cpu', type=float, default=0.75)
    ap.add_argument('--max-commit', type=float, default=0.70)
    ap.add_argument('--graph-also', action='append', default=[])
    ap.add_argument('--graph-only', action='store_true')
    ap.add_argument('--old-env', action='append', default=[], metavar='K=V')
    ap.add_argument('--new-env', action='append', default=[], metavar='K=V')
    a = ap.parse_args()

    env_for = {'old': dict(kv.split('=', 1) for kv in a.old_env), 'new': dict(kv.split('=', 1) for kv in a.new_env)}
    scratch = Path(tempfile.mkdtemp(prefix='cg-eff-'))
    failed = 0

    def check(ok, line):
        nonlocal failed
        failed += not ok
        print(('PASS ' if ok else 'FAIL ') + line, flush=True)

    try:
        trees = [a.source] + a.graph_also
        for index, tree in enumerate(trees):
            old_db, new_db = scratch / f'old{index}.db', scratch / f'new{index}.db'
            measure(a.old, tree, env_extra=env_for['old'], db_out=old_db)
            measure(a.new, tree, env_extra=env_for['new'], db_out=new_db)
            check(identical(old_db, new_db), f'graph identical on {tree}')
        if a.graph_only:
            return 1 if failed else 0

        runs = {'old': [], 'new': []}
        for i in range(a.runs):
            for name, exe in (('old', a.old), ('new', a.new)):
                usage = measure(exe, a.source, env_extra=env_for[name])
                runs[name].append(usage)
                print(f'run {i + 1} {name}: wall {usage["wall_s"]:.1f} s, cpu {usage["cpu_s"]:.1f} s, '
                      f'commit {usage["peak_commit_mb"]:.0f} MB, ws {usage["peak_ws_mb"]:.0f} MB', flush=True)
        median = lambda name, key: statistics.median(u[key] for u in runs[name])
        for key, label, limit in (('wall_s', 'wall', a.max_wall), ('cpu_s', 'CPU-seconds', a.max_cpu),
                                  ('peak_commit_mb', 'peak commit', a.max_commit)):
            old, new = median('old', key), median('new', key)
            check(new <= limit * old, f'{label}: new {new:.1f} vs old {old:.1f} = {new / old:.2f}x (limit {limit:.2f}x)')
        return 1 if failed else 0
    finally:
        import shutil
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
