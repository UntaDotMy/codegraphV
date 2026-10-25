"""Gate a bench report's index numbers: Rust no slower than TS, CPU within a budget.

    python scripts/bench/gate_index.py REPORT.json --faster tokio,codegraph \
        --cpu-budget tokio=13141 --cpu-budget codegraph=138406

Exits 0 when every check passes; prints one line per check either way.
"""
import argparse
import json
import sys


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('report')
    ap.add_argument('--faster', default='', help='repos where rust index wall_ms must be <= ts')
    ap.add_argument('--cpu-budget', action='append', default=[], metavar='REPO=MS', help='max rust index cpu_ms')
    ap.add_argument('--max-core-share', type=float, default=0.5, help='max cpu_ms/wall_ms as a share of the machine cores')
    args = ap.parse_args()

    report = json.load(open(args.report, encoding='utf-8'))
    cpus = report['machine']['cpus']
    failed = 0

    def check(ok: bool, line: str):
        nonlocal failed
        failed += not ok
        print(('PASS ' if ok else 'FAIL ') + line)

    for repo in filter(None, args.faster.split(',')):
        ix = report['repos'][repo]['index']
        check(ix['rust']['wall_ms'] <= ix['ts']['wall_ms'],
              f"{repo}: rust wall {ix['rust']['wall_ms']:.0f}ms <= ts {ix['ts']['wall_ms']:.0f}ms")
    for spec in args.cpu_budget:
        repo, budget = spec.split('=')
        cpu = report['repos'][repo]['index']['rust']['cpu_ms']
        check(cpu <= float(budget), f'{repo}: rust cpu {cpu:.0f}ms <= {budget}ms')
    for repo, data in report['repos'].items():
        rust = data['index'].get('rust')
        if rust:
            cores = rust['cpu_ms'] / max(rust['wall_ms'], 1)
            limit = cpus * args.max_core_share
            check(cores <= limit, f'{repo}: rust avg cores {cores:.2f} <= {limit:g}')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
