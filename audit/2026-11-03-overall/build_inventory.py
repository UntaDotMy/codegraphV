"""Rebuild audit inventories; lexical declarations are not coverage claims."""
import collections
import csv
import json
import pathlib
import re
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = pathlib.Path(__file__).resolve().parent
files = subprocess.check_output(['git', 'ls-files'], cwd=ROOT, text=True).splitlines()
rows = []
summary = collections.defaultdict(lambda: dict(files=0, lines=0, declarations=0))
native = []
for name in files:
    p = ROOT / name
    if not p.is_file():
        continue
    group = name.split('/')[1] if name.startswith('crates/') else name.split('/')[0]
    if not name.endswith(('.rs', '.ts', '.tsx', '.js', '.mjs', '.svelte', '.py', '.ps1', '.sh')):
        continue
    text = p.read_text(encoding='utf-8', errors='replace')
    lines = text.splitlines()
    summary[group]['files'] += 1
    summary[group]['lines'] += len(lines)
    # Captures Rust fn declarations (including tests and nested functions). The
    # reference inventory is lexical, too: closures and generated code need AST review.
    pattern = r'\bfn\s+([\w]+)\s*(?:<|\()' if name.endswith('.rs') else r'\b(?:function|def)\s+([\w]+)\s*\('
    for line, value in enumerate(lines, 1):
        for match in re.finditer(pattern, value):
            row = dict(path=name, line=line, symbol=match[1], review='inventory-only; not individually certified')
            rows.append(row)
            summary[group]['declarations'] += 1
            if name.startswith('crates/') and '/src/' in name and name.endswith('.rs'):
                native.append(row)
with (OUT / 'function-inventory.csv').open('w', encoding='utf-8', newline='') as f:
    writer = csv.DictWriter(f, fieldnames=['path', 'line', 'symbol', 'review'])
    writer.writeheader()
    writer.writerows(rows)
(OUT / 'inventory.json').write_text(json.dumps(dict(
    baseline=subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
    scope='Tracked source inventory, including reference/tests/vendor. Lexical, not AST-complete or function coverage.',
    native_src_declarations=len(native), groups=summary), indent=2), encoding='utf-8')
ledger = json.loads((ROOT / 'docs/benchmarks/audit-2026-11-03/upstream.json').read_text())
out = ['# Complete upstream disposition ledger', '',
       'Inherited classifications from the v0.0.9 audit; live HEAD rechecked in this audit. Pending means not claimed ported, not a reproduced native bug.', '',
       f"Reviewed through `{ledger['reviewedThrough']}`; previous port `{ledger['previousPort']}`.", '',
       '| Commit | Status | Change | Upstream regression / files |', '|---|---|---|---|']
for c in ledger['commits']:
    tests = [p for p in c['files'] if p.startswith('__tests__/')]
    evidence = tests or [p for p in c['files'] if p != 'CHANGELOG.md']
    out.append('| [{}](https://github.com/colbymchenry/codegraph/commit/{}) | {} | {} | {} |'.format(c['revision'][:8],c['revision'],c['status'],c['subject'].replace('|','/'),'<br>'.join(evidence).replace('|','/')))
(OUT / 'upstream-ledger.md').write_text('\n'.join(out)+'\n',encoding='utf-8')
print(json.dumps({'native_src_declarations':len(native),'all_lexical_declarations':len(rows),'upstream':dict(collections.Counter(c['status'] for c in ledger['commits']))}))
