"""Unit tests for scripts/bench/bench.py: python scripts/bench/test_bench.py"""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench  # noqa: E402

RUST = '''\
pub struct Deserializer<R> {
    read: R,
}

impl<R> Deserializer<R> {
    pub(crate) fn parse_number(&mut self) -> u64 {
        self.parse_number_inner()
    }
}

pub fn from_str(s: &str) -> u64 {
    let d = Deserializer::from_str(s);
    parse_number(d)
}
macro_rules! tri { () => {} }
'''

TS = '''\
export class ToolHandler {
  private async handleExplore(args: Record<string, unknown>): Promise<string> {
    return this.handleExplore(args);
  }
  getBudget = (n: number): number => n * 2;
}
export function getExploreBudget(fileCount: number): number {
  if (fileCount < 500) return 1;
  return getExploreBudget(fileCount - 1);
}
export const CODEGRAPH_BLOCK = `x`;
'''


class GroundTruth(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        (root / 'src').mkdir()
        (root / 'src' / 'de.rs').write_text(RUST, encoding='utf-8')
        (root / 'src' / 'tools.ts').write_text(TS, encoding='utf-8')
        # an index directory must never be read as source
        (root / '.codegraph').mkdir()
        (root / '.codegraph' / 'decoy.rs').write_text('pub fn from_str() {}\n', encoding='utf-8')
        self.root = root

    def tearDown(self):
        self.tmp.cleanup()

    def test_rust_definitions_not_calls(self):
        self.assertEqual(bench.find_definitions(self.root, 'Deserializer'), [('src/de.rs', 1)])
        self.assertEqual(bench.find_definitions(self.root, 'parse_number'), [('src/de.rs', 6)])
        self.assertEqual(bench.find_definitions(self.root, 'from_str'), [('src/de.rs', 11)])
        self.assertEqual(bench.find_definitions(self.root, 'tri'), [('src/de.rs', 15)])

    def test_ts_definitions_not_calls(self):
        self.assertEqual(bench.find_definitions(self.root, 'ToolHandler'), [('src/tools.ts', 1)])
        self.assertEqual(bench.find_definitions(self.root, 'handleExplore'), [('src/tools.ts', 2)])
        self.assertEqual(bench.find_definitions(self.root, 'getBudget'), [('src/tools.ts', 5)])
        self.assertEqual(bench.find_definitions(self.root, 'getExploreBudget'), [('src/tools.ts', 7)])
        self.assertEqual(bench.find_definitions(self.root, 'CODEGRAPH_BLOCK'), [('src/tools.ts', 11)])

    def test_path_hint_filters(self):
        self.assertEqual(bench.find_definitions(self.root, 'from_str', 'src/tools'), [])

    def test_index_directory_is_never_opened(self):
        opened = []
        real = bench.read_lines
        bench.read_lines = lambda p: (opened.append(p), real(p))[1]
        try:
            bench.find_definitions(self.root, 'from_str')
        finally:
            bench.read_lines = real
        self.assertTrue(opened)
        self.assertFalse([p for p in opened if '.codegraph' in p.parts])


class OutputParsing(unittest.TestCase):
    TS_OUT = ('**Exploration: q**\n\n- `Deserializer` (src/de.rs:31) — 6 callers\n\n'
              '**`src/de.rs`** — eat_char(calls)\n\n```rust\n28\t//\n31\tpub struct Deserializer<R> {\n```\n')
    RUST_OUT = ('**Explore: q**\n\n**src/de.rs**\n- parse_number (method) :509–528 `(..)`\n\n'
                '```rust\n    29\t\n    31\tpub struct Deserializer<R> {\n```\n')

    def test_ts_format(self):
        cov = bench.parse_explore_output(self.TS_OUT)
        self.assertIn(('src/de.rs', 31), cov.shown)
        self.assertNotIn(('src/de.rs', 29), cov.shown)
        self.assertTrue(cov.is_located(('src/de.rs', 31)))

    def test_rust_format(self):
        cov = bench.parse_explore_output(self.RUST_OUT)
        self.assertIn(('src/de.rs', 31), cov.shown)
        self.assertFalse(cov.is_shown(('src/de.rs', 509)))
        self.assertTrue(cov.is_located(('src/de.rs', 509)))

    def test_score(self):
        cov = bench.parse_explore_output(self.RUST_OUT)
        shown, located = bench.score(cov, [[('src/de.rs', 31)], [('src/de.rs', 509)], [('src/x.rs', 1)]])
        self.assertAlmostEqual(shown, 1 / 3)
        self.assertAlmostEqual(located, 2 / 3)

    def test_query_identifiers(self):
        self.assertEqual(bench.query_identifiers('Map.entry or_insert a Map'), ['Map', 'entry', 'or_insert'])


class GrepHits(unittest.TestCase):
    """`rg` lists hits in directory-walk order, which varies between runs because its parallel
    walkers finish in a different order each time. The arm keeps the first `GREP_HEAD_LIMIT`
    lines, so the same query could read different text — and score differently — on every run.
    Picking is by sort, never by `rg --sort=path`: that costs about a second on a large
    repository and is itself unstable (measured 0.5–5.0 s), which would poison the wall time
    the arm reports."""

    HITS = ''.join(f'src/f{i:03d}.rs:{i + 1}:fn alpha{i}() {{\n' for i in range(300))

    def test_the_same_hits_in_another_order_pick_the_same_lines(self):
        forward = self.HITS.splitlines()
        backward = list(reversed(forward))
        self.assertEqual(bench.head_lines('\n'.join(forward)), bench.head_lines('\n'.join(backward)))

    def test_the_first_limit_lines_are_kept_in_a_stable_path_order(self):
        got = bench.head_lines(self.HITS)
        self.assertEqual(len(got), bench.GREP_HEAD_LIMIT)
        self.assertEqual(got[0], 'src/f000.rs:1:fn alpha0() {')
        self.assertEqual(got[-1], f'src/f{bench.GREP_HEAD_LIMIT - 1:03d}.rs:{bench.GREP_HEAD_LIMIT}:fn alpha{bench.GREP_HEAD_LIMIT - 1}() {{')

    def test_a_short_answer_is_returned_whole(self):
        short = 'src/a.rs:1:x\nsrc/b.rs:2:y\n'
        self.assertEqual(bench.head_lines(short), ['src/a.rs:1:x', 'src/b.rs:2:y'])

    def test_crlf_is_not_a_new_line(self):
        self.assertEqual(bench.head_lines('src/a.rs:1:x\r\nsrc/b.rs:2:y\r\n'), ['src/a.rs:1:x', 'src/b.rs:2:y'])

    def test_the_limit_is_stable_when_two_identifiers_see_different_orders(self):
        # A line's rank comes from its path, then its number, so an interleaved rg order and a
        # file-major one select the same first N.
        interleaved = '\n'.join(f'dir{i % 4}/f{i:03d}.rs:{i}:fn a() {{' for i in range(400))
        file_major = '\n'.join(sorted(interleaved.splitlines(), key=lambda ln: (ln.split(':')[0], int(ln.split(':')[1]))))
        self.assertEqual(bench.head_lines(interleaved), bench.head_lines(file_major))


if __name__ == '__main__':
    unittest.main()
