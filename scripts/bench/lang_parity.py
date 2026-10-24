"""Index one repository with the TS engine and the Rust binary and diff the two graphs.

    python scripts/bench/lang_parity.py SOURCE_DIR [--show 10] [--keep]

SOURCE_DIR is copied (without .git, .codegraph, node_modules) into a temporary directory,
indexed by `node dist/bin/codegraph.js init -y .`, its database moved aside, then indexed by
target/release/codegraph; compare_index.py prints the difference. Exits with compare_index's
status (0 = identical graphs). The temporary copy is deleted unless --keep.
"""
import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bench  # noqa: E402

SKIP = shutil.ignore_patterns('.git', '.codegraph', 'node_modules')


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('source')
    ap.add_argument('--show', default='10')
    ap.add_argument('--keep', action='store_true')
    a = ap.parse_args()
    work = Path(tempfile.mkdtemp(prefix='cg-parity-'))
    try:
        repo = work / 'repo'
        shutil.copytree(a.source, repo, ignore=SKIP)
        ts = bench.measure(bench.ts_cmd() + ['init', '-y', '.'], repo)
        print(f'ts index: exit {ts.code}, {ts.wall_ms:.0f} ms')
        if ts.code != 0:
            return 2
        ts_db = work / 'ts.db'
        shutil.move(str(repo / '.codegraph' / 'codegraph.db'), ts_db)
        shutil.rmtree(repo / '.codegraph')
        rust = bench.measure([str(bench.rust_bin()), 'init', '.'], repo)
        print(f'rust index: exit {rust.code}, {rust.wall_ms:.0f} ms')
        if rust.code != 0:
            return 2
        return subprocess.run([sys.executable, str(HERE / 'compare_index.py'), str(ts_db),
                               str(repo / '.codegraph' / 'codegraph.db'), '--show', a.show]).returncode
    finally:
        if a.keep:
            print(f'kept {work}')
        else:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
