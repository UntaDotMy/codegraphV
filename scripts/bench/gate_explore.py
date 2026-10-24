"""Gate a bench report's explore numbers: Rust recall and token use against TS.

    python scripts/bench/gate_explore.py REPORT.json --recall --tokens

--recall: Rust mean recall >= TS mean recall on every repo.
--tokens: Rust median out_tokens <= TS median out_tokens on every repo.
--latency REPO=MS: Rust median wall_ms < MS on REPO (repeatable); with any --latency, Rust
    median wall_ms <= the rg arm's on every repo that has one.
Exits 0 when every requested check passes.
"""
import argparse
import json
import statistics
import sys


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('report')
    ap.add_argument('--recall', action='store_true')
    ap.add_argument('--tokens', action='store_true')
    ap.add_argument('--latency', action='append', default=[], metavar='REPO=MS')
    args = ap.parse_args()
    report = json.load(open(args.report, encoding='utf-8'))
    failed = 0
    for repo, data in report['repos'].items():
        arms = {arm: [q for q in rows if 'recall' in q] for arm, rows in data['arms'].items()}
        rust, ts, rg = arms.get('rust'), arms.get('ts'), arms.get('rg')
        if args.latency and rust:
            wall = statistics.median(q['wall_ms'] for q in rust)
            for spec in args.latency:
                name, limit = spec.split('=')
                if name == repo:
                    ok = wall < float(limit)
                    failed += not ok
                    print(f"{'PASS' if ok else 'FAIL'} {repo}: rust median wall {wall:.0f}ms < {limit}ms")
            if rg:
                rg_wall = statistics.median(q['wall_ms'] for q in rg)
                ok = wall <= rg_wall
                failed += not ok
                print(f"{'PASS' if ok else 'FAIL'} {repo}: rust median wall {wall:.0f}ms <= rg {rg_wall:.0f}ms")
        if not (args.recall or args.tokens):
            continue
        if not rust or not ts:
            print(f'FAIL {repo}: report lacks a rust or ts arm')
            failed += 1
            continue
        if args.recall:
            r, t = statistics.mean(q['recall'] for q in rust), statistics.mean(q['recall'] for q in ts)
            ok = r >= t
            failed += not ok
            print(f"{'PASS' if ok else 'FAIL'} {repo}: rust recall {r:.2f} >= ts {t:.2f}")
        if args.tokens:
            r, t = statistics.median(q['out_tokens'] for q in rust), statistics.median(q['out_tokens'] for q in ts)
            ok = r <= t
            failed += not ok
            print(f"{'PASS' if ok else 'FAIL'} {repo}: rust median tokens {r:.0f} <= ts {t:.0f}")
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
