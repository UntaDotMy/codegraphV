# Review instructions

## Always check

- An installer target change without matching coverage in `crates/codegraph-installer/tests/contracts.rs`.
- A router, web framework, or `WHEN` rule change without the same change to `docs/design/framework-coverage.md`.
- A `NodeKind` or `EdgeKind` string that differs from `crates/codegraph-types/src/kinds.rs`. Extractors and resolvers use those exact strings.
- An image in `README.md` whose bytes change without its `?v=N` bumped.
