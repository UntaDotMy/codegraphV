"""Gate: several big source files (up to the 10 MB limit) do not blow up indexing memory.

    python scripts/bench/memory_budget_gate.py EXE [--big-files 12] [--big-mb 8] [--max-extra-mb 600]

Builds a small TypeScript project, and the same project plus N generated source files of
`--big-mb` MB each. Both are indexed with EXE; the peak commit of the second may exceed the
first by at most --max-extra-mb. tree-sitter needs roughly 10x a file's size while parsing it, so
without a byte budget on files in flight every worker could hold one at once. Exits 0 on success.
"""
import argparse
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from proc_measure import measure  # noqa: E402


def write_small_project(root: Path):
    root.mkdir(parents=True, exist_ok=True)
    for i in range(40):
        (root / f'mod{i:02}.ts').write_text(
            ''.join(f'export function f{i}_{j}(a: number) {{ return a + {j}; }}\n' for j in range(200)), encoding='utf-8')


def write_big_file(path: Path, mb: float, tag: int):
    target = int(mb * 1048576)
    parts, size, j = [], 0, 0
    while size < target:
        line = f'export function big{tag}_{j:07}(a: number, b: string) {{ return a + b.length + {j}; }}\n'
        parts.append(line)
        size += len(line)
        j += 1
    path.write_text(''.join(parts), encoding='utf-8')


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('exe')
    ap.add_argument('--big-files', type=int, default=12)
    ap.add_argument('--big-mb', type=float, default=8)
    ap.add_argument('--max-extra-mb', type=float, default=600)
    a = ap.parse_args()
    scratch = Path(tempfile.mkdtemp(prefix='cg-membudget-'))
    try:
        small, big = scratch / 'small', scratch / 'big'
        write_small_project(small)
        write_small_project(big)
        for n in range(a.big_files):
            write_big_file(big / f'generated{n:02}.ts', a.big_mb, n)
        base = measure(a.exe, small)
        with_big = measure(a.exe, big)
        extra = with_big['peak_commit_mb'] - base['peak_commit_mb']
        print(f'small tree: commit {base["peak_commit_mb"]:.0f} MB, wall {base["wall_s"]:.1f} s')
        print(f'+{a.big_files} x {a.big_mb:g} MB files: commit {with_big["peak_commit_mb"]:.0f} MB, wall {with_big["wall_s"]:.1f} s')
        ok = extra <= a.max_extra_mb
        print(('PASS' if ok else 'FAIL') + f' extra memory {extra:.0f} MB (limit {a.max_extra_mb:.0f} MB)')
        return 0 if ok else 1
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
