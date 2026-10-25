#!/usr/bin/env python3
"""Diff two codegraph.db files: nodes, edges and unresolved refs as multisets.

    python scripts/bench/compare_index.py BASE.db NEW.db [--show 10]

Exit 0 when identical, 1 otherwise. Node ids are content hashes, so rows
compare directly; `updated_at` and row ids are ignored.
"""
import argparse
import sqlite3
import sys
from collections import Counter

TABLES = {
    'nodes': 'id, kind, name, qualified_name, file_path, language, start_line, end_line, start_column, end_column, '
             'docstring, signature, visibility, is_exported, is_async, is_static, is_abstract, decorators, '
             'type_parameters, return_type',
    'edges': 'source, target, kind, metadata, line, col, provenance',
    'unresolved_refs': 'from_node_id, reference_name, reference_kind, line, col, candidates, file_path, language, status',
}


def rows(db: str, table: str) -> Counter:
    con = sqlite3.connect(f'file:{db}?mode=ro', uri=True)
    try:
        return Counter(con.execute(f'SELECT {TABLES[table]} FROM {table}'))
    finally:
        con.close()


def diff(base: str, new: str) -> dict:
    out = {}
    for t in TABLES:
        a, b = rows(base, t), rows(new, t)
        out[t] = (a - b, b - a, sum(a.values()), sum(b.values()))
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('base')
    ap.add_argument('new')
    ap.add_argument('--show', type=int, default=5)
    a = ap.parse_args(argv)
    same = True
    for t, (missing, extra, na, nb) in diff(a.base, a.new).items():
        n_miss, n_extra = sum(missing.values()), sum(extra.values())
        print(f'{t}: base {na}, new {nb}, missing {n_miss}, extra {n_extra}')
        for label, c in (('-', missing), ('+', extra)):
            for row in list(c)[:a.show]:
                print(f'  {label} {row}')
        same &= n_miss == 0 and n_extra == 0
    print('IDENTICAL' if same else 'DIFFERENT')
    return 0 if same else 1


if __name__ == '__main__':
    sys.exit(main())
