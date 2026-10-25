"""Index bench repos fresh with the Rust binary and diff each against a baseline index.

    python scripts/bench/verify_identical.py BASELINE_DIR [repo[@rev] ...]

BASELINE_DIR holds <repo>.db files (copies of a known-good .codegraph/codegraph.db); the
fresh copies go next to it and are deleted afterwards.
`codegraph@937a233f` indexes this repository at that commit, which must be the
source the baseline was built from. Exits 0 when every repo is IDENTICAL.
"""
import json
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench  # noqa: E402

HERE = Path(__file__).resolve().parent


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    baseline = Path(sys.argv[1])
    specs = {r['name']: r for r in json.load(open(HERE / 'queries.json', encoding='utf-8'))['repos']}
    targets = sys.argv[2:] or list(specs)
    work = baseline.parent
    failed = 0
    for target in targets:
        name, _, rev = target.partition('@')
        spec = dict(specs[name])
        if rev:
            spec['rev'] = rev
        dest = work / f'verify-{name}'
        bench.prepare_copy(spec, dest)
        run = bench.measure([str(bench.rust_bin()), 'init', '.'], dest)
        print(f'{name}: index exit {run.code}, wall {run.wall_ms:.0f}ms, cpu {run.cpu_ms:.0f}ms')
        diff = subprocess.run([sys.executable, str(HERE / 'compare_index.py'), str(baseline / f'{name}.db'),
                               str(dest / bench.INDEX_DIRS['rust'] / 'codegraph.db')], capture_output=True, text=True)
        print(diff.stdout.strip())
        failed += run.code != 0 or diff.returncode != 0
        shutil.rmtree(dest, ignore_errors=True)
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
