"""Compare weighted BM25 sorting with FTS5's ranked iterator on an existing index.

Read-only; never indexes or modifies the project. Timings cover SQL execution
and materializing results, not MCP/model latency. Uses Python's SQLite build.

    python scripts/bench/fts_rank.py .codegraphV/codegraph.db
"""
import argparse
import json
from pathlib import Path
import sqlite3
import statistics
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", type=Path)
    parser.add_argument("--repeats", type=int, default=30)
    parser.add_argument("--query", action="append")
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    old = "SELECT nodes.*, bm25(nodes_fts, 0, 20, 5, 1, 2) AS score FROM nodes_fts JOIN nodes ON nodes_fts.id = nodes.id WHERE nodes_fts MATCH ? ORDER BY score LIMIT 100"
    new = "SELECT nodes.*, nodes_fts.rank AS score FROM nodes_fts JOIN nodes ON nodes_fts.id = nodes.id WHERE nodes_fts MATCH ? AND nodes_fts.rank MATCH 'bm25(0, 20, 5, 1, 2)' ORDER BY nodes_fts.rank LIMIT 100"
    reports = []
    with sqlite3.connect(args.database.resolve().as_uri() + "?mode=ro", uri=True) as db:
        # The live watcher may update the index; both arms must see one snapshot.
        db.execute("BEGIN")
        for query in args.query or ['"get"*', '"render"*', '"search"*', '"node"*']:
            expected = db.execute(old, (query,)).fetchall()
            actual = db.execute(new, (query,)).fetchall()
            if actual != expected:
                raise RuntimeError(f"Ranked results differ for {query!r}")
            times = {"bm25": [], "rank": []}
            # Alternate order to reduce warm-cache/order bias; discard warm-ups.
            for repeat in range(args.repeats + 3):
                arms = [("bm25", old), ("rank", new)]
                for name, sql in arms[::1 if repeat % 2 else -1]:
                    start = time.perf_counter_ns()
                    db.execute(sql, (query,)).fetchall()
                    if repeat >= 3:
                        times[name].append((time.perf_counter_ns() - start) / 1e6)
            medians = {name + "_median_ms": round(statistics.median(rows), 3)
                       for name, rows in times.items()}
            reports.append({"query": query, "rows": len(actual), **medians})
        print(json.dumps({"sqlite_version": sqlite3.sqlite_version, "projection": "full node rows and scores",
                          "repeats": args.repeats, "queries": reports}, indent=2))


if __name__ == "__main__":
    main()
