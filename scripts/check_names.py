"""The Rust build is codegraphV: check (or fix) that no user-visible text still uses the
original codegraph's names.

    python scripts/check_names.py          # exit 1 and list every old name found
    python scripts/check_names.py --fix    # rename them in place (string literals only)

Scanned: the string literals of crates/*/src/**/*.rs (never identifiers, comments or crate
names — `CodeGraph` the struct and `codegraph_core` the crate stay) and the text assets the
binary embeds (crates/*/src/assets/*). TypeScript-generated test fixtures are not scanned: they
are the original's recorded output, compared through the same name map.

ALLOWED lists the places that must name the original on purpose: migrate and the installer
recognizing and removing what the original wrote, and the scanner skipping its index folder.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SUBCOMMANDS = ('init|index|sync|uninit|status|query|explore|node|files|callers|callees|impact|affected|'
               'context|daemon|unlock|install|uninstall|upgrade|version|serve|prompt-hook|ui|web|migrate')
TOOLS = 'explore|node|search|callers|callees|impact|files|status|context|trace'
# Not inside a longer name or a path. A `\n`, `\r` or `\t` escape in a literal still starts a word.
BEFORE = r'(?:(?<=\\[nrt])|(?<![\w/@.\-]))'
RULES = [
    (re.compile(r'mcp__codegraph__'), 'mcp__codegraphV__'),
    (re.compile(BEFORE + r'codegraph_(' + TOOLS + r')\b'), r'codegraphV_\1'),
    (re.compile(r'(?<=</)codegraph_(' + TOOLS + r')\b'), r'codegraphV_\1'),  # a closing tag
    (re.compile(r'\bCODEGRAPH_'), 'CODEGRAPHV_'),  # never matches CODEGRAPHV_: there V precedes the _
    (re.compile(r'CodeGraph(?!V)'), 'CodeGraphV'),
    (re.compile(r'\bCodegraph(?!V)\b'), 'CodeGraphV'),
    (re.compile(r'\[codegraph\]'), '[codegraphV]'),
    (re.compile(BEFORE + r'codegraph (?=(' + SUBCOMMANDS + r')\b)'), 'codegraphV '),
    (re.compile(r'(?<![\w/@-])\.codegraph(?=[/\\"`\' ]|$)'), '.codegraphV'),
    (re.compile(BEFORE + r'codegraph(?=\.exe\b)'), 'codegraphV'),
    # The bare product / command / server name, last so the rules above see their forms first.
    (re.compile(BEFORE + r'codegraph(?![\w.\-])'), 'codegraphV'),
]

# (path suffix, exact literal, or None for the whole file): the original's names, on purpose.
ALLOWED = [
    ('crates/codegraph-types/src/paths.rs', '.codegraph'),                      # the original's index folder: skipped, migrated
    ('crates/codegraph-resolve/src/workspace_packages.rs', '.codegraph'),      # the original's index folder: skipped
    ('crates/codegraph-core/src/file_limits.rs', 'codegraph:oversize:{size}'),  # hashed into indexes; must equal the original's
    ('crates/codegraph-types/src/branding.rs', None),                           # the name map itself
    ('crates/codegraph-installer/src/original.rs', None),                      # what the original wrote (migrate)
    ('crates/codegraph-cli/src/migrate.rs', None),                              # migrate from the original
]


def literals(src):
    """(start, end) of every string literal body, skipping comments and char literals."""
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith('//', i):
            j = src.find('\n', i)
            i = n if j < 0 else j
            continue
        if src.startswith('/*', i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith('/*', i):
                    depth, i = depth + 1, i + 2
                elif src.startswith('*/', i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            continue
        word_start = i == 0 or not (src[i - 1].isalnum() or src[i - 1] == '_')
        m = re.match(r'b?r(#*)"', src[i:i + 40]) if c in 'br' and word_start else None
        if m:
            hashes = m.group(1)
            start = i + m.end()
            end = src.find('"' + hashes, start)
            yield start, end
            i = end + 1 + len(hashes)
            continue
        if c == '"' or (c == 'b' and src.startswith('b"', i) and word_start):
            start = i + (2 if c == 'b' else 1)
            j = start
            while src[j] != '"':
                j += 2 if src[j] == '\\' else 1
            yield start, j
            i = j + 1
            continue
        if c == "'":
            m = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'])'", src[i:i + 16])
            i += m.end() if m else 1
            continue
        i += 1


def rename(text):
    for pattern, repl in RULES:
        text = pattern.sub(repl, text)
    return text


def allowed(rel, body):
    return any(rel.endswith(path) and (exact is None or body == exact) for path, exact in ALLOWED)


def main():
    fix = '--fix' in sys.argv
    found = 0
    sources = sorted(p for p in ROOT.glob('crates/*/src/**/*.rs'))
    assets = sorted(p for p in ROOT.glob('crates/*/src/assets/*') if p.is_file())
    for path in sources + assets:
        rel = path.relative_to(ROOT).as_posix()
        with path.open(encoding='utf8', newline='') as f:  # keep CRLF files CRLF
            src = f.read()
        spans = list(literals(src)) if path.suffix == '.rs' else [(0, len(src))]
        out, last = [], 0
        for start, end in spans:
            body = src[start:end]
            new = body if allowed(rel, body) else rename(body)
            if new != body:
                found += 1
                line = src.count('\n', 0, start) + 1
                print(f'{rel}:{line}: {body[:100]!r}')
            out += [src[last:start], new]
            last = end
        out.append(src[last:])
        if fix and ''.join(out) != src:
            with path.open('w', encoding='utf8', newline='') as f:
                f.write(''.join(out))
    verb = 'renamed' if fix else 'found'
    print(f'{found} literal(s) with the original\'s names {verb}')
    return 0 if fix or found == 0 else 1


if __name__ == '__main__':
    sys.stdout.reconfigure(encoding='utf8')
    sys.exit(main())
