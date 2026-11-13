# Validation evidence

Baseline `8182ee403d2ca64a2516cf67f0b4f3a2bbd0fc79` (v0.0.9), 2026-11-03, Windows x64. The working tree was clean before this audit. Only the new `audit/` handoff package was added; existing production files were not edited.

| Check | Result | Interpretation |
|---|---|---|
| `cargo test --workspace --locked` | Exit 0; **837 passed, 0 failed, 1 ignored**, across 106 reported unit/integration/doc-test result blocks | Fresh run on this machine; includes native installer, migration, explore and runtime suites. Does not prove all upstream regressions or host integrations. |
| `python audit/2026-11-03-overall/probe_lifecycle.py` | Exit 0; all three defect scenarios reproduced | Current debug executable copied to temp directory; home/profile variables and project isolated. No real integration settings changed. See `lifecycle-probes.json`. |
| `node scripts/rust-golden/check-upstream.mjs` | Exit 1, changed fingerprints across groups | Expected review-required result against `c8a80149`, not a new differential failure. Broad parity checkpoint remains stale. |
| `cargo fmt --all -- --check` | Exit 1, formatting diffs | Existing formatting debt; no formatter changes applied. |
| `git diff --check` | Exit 0 | Tracked source unchanged. This does not validate new untracked report files by itself. |
| `git ls-remote https://github.com/colbymchenry/codegraph.git HEAD refs/heads/main` | Both returned `f72c9d2efb14454ff4f9fd7b8460abd82f19a287` | Live remote recheck, using approved network access after sandbox networking failed. No remote ref or branch modified. |
| Windows bootstrap recovery script | **Not validated in this environment** | Plain invocation refused by PowerShell execution policy. Process-scoped `-ExecutionPolicy Bypass` reached the script, but `Get-FileHash` was unavailable and it stopped before the intended copy-failure case. Do not count this as a product recovery failure or pass. Earlier audit's successful run is historical evidence only. |

Fresh workspace log: [workspace-tests.log](workspace-tests.log). Root command output was originally captured as `audit-current-workspace.log` (PowerShell UTF-16); the included copy is UTF-8. The existing compiler/linker warnings remain in the log.

Not run: a full fresh TypeScript-oracle differential matrix, fresh real-agent A/B, Linux/macOS execution, ARM artifacts, actual host GUI integration, CVE database scan, fuzzing, coverage instrumentation or an actual online install/upgrade/uninstall. No claim of every-function correctness is made. This audit is designed to give the implementation agent reproducible first fixes and an explicit remaining review queue.

The baseline includes previous parity evidence in `docs/benchmarks/audit-2026-11-03/report.md`. That is cited as prior evidence rather than relabeled as work executed in this pass.

Follow-up preservation audit: reran `probe_lifecycle.py` with 16 local instruction-file fixture sequences, a fenced-marker example and ordinary sibling hooks. Normal LF/CRLF append/refresh/removal and ordinary hook merging passed. Fenced example replacement, missing-final-newline round-trip differences and deletion of whitespace-only originals were reproduced; previous three lifecycle defects remain reproduced. Full Rust suite was not repeated because only audit documents/probes changed. See `preservation-plan.md` for limits and `production-plan.md` for the newly requested optimization/production-quality workstreams; those planned gains have not been benchmarked or implemented.

## Implementation pass validation (2026-11-03, working tree)

All commands below ran on the same Windows x64 machine, with isolated temporary homes/projects
for installer, migration and uninstall tests. The baseline commit was not modified; the
implementation changes are uncommitted in the working tree.

| Check | Result | Evidence |
|---|---|---|
| `cargo test --workspace --locked` | **Exit 0; 933 passed, 0 failed** across 132 reported result blocks | `workspace-final-tenth.log`; adds `python_receiver_rebinding`, `python_multiline_bases`, `python_pep695_bases`, `python_outside_bases`, `default_export_template` and the `serve --no-telemetry` spawn to the prior suite |
| `cargo test -p codegraph-mcp --test explore_render` | Exit 0; 18 passed | `explore-render-final-tenth.log` |
| `cargo test -p codegraph-cli --test migrate` | Exit 0; 9 passed | `migrate-final-tenth.log` |
| `cargo build --release --locked -p codegraph-cli` | Exit 0; binary reports `codegraphV 0.0.9` | `release-final-tenth.log` |
| `cargo fmt --all -- --check` | Exit 1, pre-existing workspace formatting debt (133,058 diff lines). No workspace reformat applied; new code follows the surrounding style. | `fmt-final-tenth.log` |
| `node scripts/rust-golden/check-upstream.mjs` | Exit 1, review-required fingerprints (unchanged expectation; not a differential failure) | `parity-final-tenth.log` |
| Before/after graph benchmark | Baseline `8182ee40` vs working tree on the same DB: callers +2.4%, impact +4.7%, flow âˆ’2.0%, bfs +7.6%, search +8.8% (repeat runs within noise), identical counts; 40k-caller fanout 1,351 ms â†’ 384 ms (âˆ’72%) with the intentional 10k safety cap | `bench-before-after.md`, raw `bench-*.json` |
| Candidate index measurement | 2,000-file synthetic TypeScript corpus: 7.1 s cold, ~2.0 s forced reindex, peak RSS ~46 MB. Candidate-only; no baseline index binary. | `bench-before-after.md` |
| Sensitivity probes | Migration waiter reproduces `UNIQUE constraint failed: schema_versions.version` without `BEGIN IMMEDIATE`; Cargo manifest retarget test fails without the new fingerprint; C++ reentry test crashed with 0xC00000FD before the guard; the `this`-receiver regression caught the `self` normalization; the Spring overload test failed before the containment preference; the Liquid vector changed only for the intentionally fixed hyphen names (`bad-name`/`ghost-name` indexed, `tight-` still not). | `implementation-logs/`, pre-fix runs recorded in progress.md |

Not run / not proven: Linux and macOS execution, real-agent host integration, real-agent token
A/B, fresh real-repository differentials for the ported language regressions, real-repository
index/RSS measurement, dependency/CVE/fuzz coverage, packaged viewer assets, an executed release
attestation/SBOM run, and the C/C++ compile-database feature native does not implement. These
remain release gaps, not passes.

## Continuation evidence (2026-11-03)

2026-11-04: the daemon control validation follow-up passed three MCP library tests and four
CLI daemon integration tests (`implementation-logs/control-validation.log` and
`daemon-control-validation.log`). This includes malformed control negatives and live-daemon
responsiveness. Release rebuild output is in `release-control-final.log`; the earlier
`release-continuation.log` completed successfully. These runs do not establish cancellation
or cheap graph-lookup fairness under saturation, and do not close the remaining release gates.

Existing implementation changes were retained. Fresh executions:

| Check | Result | Evidence under `implementation-logs/` |
|---|---|---|
| Recovery sensitivity | Two failures before fix: corrupt-journal deletion and failed-restore retry loss | `recovery-followup-before.log` |
| Latest installer tests | 21 unit + 19 contract tests passed, including killed subprocess lock owner, immediate recovery, intervening edits and preservation contracts | `recovery-followup-final.log` |
| Python sensitivity | Wrapped/PEP-695 headers missed inherited calls; generic headers missed extends edges; external Enum linked a project namesake | `python-headers-before.log`, `python-outside-before.log` |
| Latest Python tests | 5 core + 1 extraction passed; full-vs-sync nodes/edges/pending refs, import negative controls and LF/CRLF computed-base negatives | `python-final.log` |
| Workspace | 920 passed, zero failed, 126 result blocks; preceded final atomic journal change, separately validated by latest installer and required-target suites | `workspace-continuation.log` |
| Explicit required targets | migrate 9 passed; explore_render 18 passed | `required-targets-continuation.log` |
| Upstream fingerprint | Exit 1; review required, not a full oracle differential run | `parity-continuation.log` |
| Formatting | Exit 1; workspace-wide differences, no mass reformat performed | `fmt-continuation.log` |

No new performance/token claim. The earlier benchmark report was corrected: medians of batch
averages are not request p95, and matching result counts do not prove matching answers.
A20 remains partial: cancellation and cheap graph-query fairness remain open. Docker/cargo-audit
are absent locally; platform, host, real-repository differential, agent A/B, advisory/fuzz and
release-artifact validation remain unexecuted. Commit/push/tag/release are authorized after
completion, but have not been performed while those gates and the backlog remain open.
Existing Python indexes require full reindexing for the extraction changes.

## Web sources consulted

- [Original releases](https://github.com/colbymchenry/codegraph/releases): v1.6.2 release date, distinction between releases and current main.
- [Pinned upstream head](https://github.com/colbymchenry/codegraph/commit/f72c9d2efb14454ff4f9fd7b8460abd82f19a287): exact comparison boundary; git remote metadata independently checked.
- [Claude hook reference](https://code.claude.com/docs/en/hooks): direct-exec `args`, shell behavior, Windows fallback, UserPromptSubmit timeout. This does not certify all historical Claude versions; implementation should declare compatibility.
- [SemVer](https://semver.org/#spec-item-11): numeric prerelease identifier ordering.
- [MCP lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle): initialization and negotiation requirements; an older offered version is not automatically a defect.
- [GitHub provenance attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations): release provenance improvement.

GitHub API/commit-page browsing was not reliable through the web tool. Live `git ls-remote` succeeded and the exact commit objects and prior commit ledger were already present locally. No claim relies on a failed API fetch.
