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


if __name__ == '__main__':
    unittest.main()
