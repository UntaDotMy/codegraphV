<div align="center">

# codegraphV

**A full rewrite in Rust of [CodeGraph](https://github.com/colbymchenry/codegraph) by [Colby McHenry](https://github.com/colbymchenry).**
One native executable: indexer, CLI and MCP server. No Node.js, no npm, no telemetry.

Windows · macOS · Linux · MIT

</div>

> **Credit where it is due.** CodeGraph was designed and built by Colby McHenry
> ([colbymchenry/codegraph](https://github.com/colbymchenry/codegraph), MIT). The ideas, the
> graph model, the MCP tools, the language extractors and the framework resolvers are all his
> work. This project ports that TypeScript engine to Rust line by line and checks the result
> against it: on the parity repos the Rust engine builds the **same graph** (same files, nodes
> and edges) as the TypeScript engine it was ported from (v1.6.2). If you want the original,
> with its website, npm package and browser viewer, use
> [colbymchenry/codegraph](https://github.com/colbymchenry/codegraph). The original README is
> kept at [`docs/upstream-README.md`](docs/upstream-README.md).

**Why the name codegraphV?** So it never collides with the original. Every name you see is
its own, and the two can be installed side by side:

| | the original | this rewrite |
|---|---|---|
| command | `codegraph` | `codegraphV` |
| MCP server and tools | `codegraph`, `codegraph_explore`, ... | `codegraphV`, `codegraphV_explore`, ... |
| per-project index | `.codegraph/` | `.codegraphV/` |
| settings and caches | `~/.codegraph` | `~/.codegraphV` |
| environment variables | `CODEGRAPH_*` | `CODEGRAPHV_*` |

A project's `codegraph.json` config file keeps its name, so one file serves both.

## What it does

codegraphV parses your code with tree-sitter and stores every symbol and every relation
between symbols (calls, imports, extends, implements, routes and more) in a local SQLite
database in `.codegraphV/`. AI coding agents (Claude Code, Cursor, Codex, opencode and others)
query it over MCP, so a question like "how does a login request reach the database?" is
answered by one `codegraphV_explore` call. Without it, the agent loops over grep and file reads.

Everything runs on your machine. The rewrite keeps:

- **The same MCP tools:** `codegraphV_explore`, `codegraphV_node`, `codegraphV_search`,
  `codegraphV_callers`, `codegraphV_callees`, `codegraphV_impact`, `codegraphV_files` and
  `codegraphV_status`.
- **The same languages:** TypeScript, JavaScript, Python, Go, Rust, Java, C#, PHP, Ruby, C,
  C++, Objective-C, Swift, Kotlin, Scala, Dart, Svelte, Vue, Astro, Liquid, Pascal/Delphi,
  Lua, Luau, R, CFML, COBOL, VB.NET, Erlang, Solidity, Terraform, Nix, ArkTS and more.
- **The same framework resolvers:** Express, Next.js, Django, Rails, Laravel, Spring, FastAPI,
  React Native, SvelteKit, Nuxt and the rest.
- **The same dynamic-dispatch bridges** (callbacks, events, React re-renders).
- **The same agent installer** for Claude Code, Cursor, Codex, opencode, Gemini, Kiro,
  Hermes, Antigravity, GitHub Copilot, IBM Bob, Factory Droid, Windsurf, Qwen Code,
  DeepSeek Harness, Grok Build, Kimi Code, Qoder CLI and Pi.
- **Automatic sync:** the index follows your edits while an agent session runs, and catches
  up on edits made while no session was running.
- **The Claude Code prompt hook,** which adds codegraphV context to "how / where / trace"
  prompts.
- **The Claude Code search guard** (new in codegraphV): in an indexed project, a Grep, Glob
  or shell search for an indexed symbol is denied until the agent has asked codegraphV
  about it this session, and the denial names the exact `codegraphV_explore` call to make.
  Plain-text searches, pipe filters, reads and edits always pass. `install` and every
  `upgrade` write it; `CODEGRAPHV_GUARD=0` in Claude Code's environment turns it off.
- **The update notice and `codegraphV upgrade`.**

## Install

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.ps1 | iex
```

**macOS / Linux:**

```bash
curl -fsSL https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.sh | sh
```

Both scripts download the release archive for your platform, check it against the release's
`SHA256SUMS`, and delete their temporary files.
There is no release archive for Apple Silicon Macs yet: `install-rust.sh` stops and prints the
command to build from source.
The Windows installer stages the replacement before switching executables and restores the
previous executable if activation fails.

- **Windows:** the binary goes in `%LOCALAPPDATA%\codegraphV\bin`, which is added to your user
  PATH.
- **macOS / Linux:** the binary goes in `~/.local/bin`.
- **Options:** set `CODEGRAPHV_VERSION` (for example `v0.0.2`) to pin a version, or
  `CODEGRAPHV_INSTALL_DIR` to pick the folder.

Then connect your agents and index a project:

```bash
codegraphV install          # detects your agents and writes their MCP config (asks first)
cd your-project
codegraphV init             # builds .codegraphV/ for this project
```

Restart the agent once after `codegraphV install`. New projects only need `codegraphV init`.

## Coming from the original codegraph

Install codegraphV as above, then run:

```bash
codegraphV migrate --dry-run   # show what would change
codegraphV migrate             # do it (asks first; --yes skips the question)
```

`migrate` works for the npm package (`@colbymchenry/codegraph`) and for this fork's v0.0.1,
which was still called `codegraph`. It does four things:

1. **Agents:** in every agent's config it removes the original's entries (its MCP server,
   tool permission, prompt hook and instructions block) and writes codegraphV's, including
   Claude Code's prompt hook. Each file it changes is backed up first to `~/.codegraphV/migrate-backup/`.
2. **Indexes:** it moves each project's `.codegraph/` to `.codegraphV/` — the current folder,
   every project Claude Code knows, and any folder you name with `--path`. Nothing is
   re-indexed. A project that already has `.codegraphV/` keeps it.
3. **The original's program:** it removes the npm package, or v0.0.1's binary and its PATH
   entry.
4. **The original's settings:** it removes `~/.codegraph`.

On this fork's v0.0.1, `codegraph upgrade` does all of this for you: it installs codegraphV
and runs `codegraphV migrate --yes`. It then reports that `codegraph` was not updated. That is
expected, because codegraphV replaced it.

If an agent session still has an index or the old binary open, `migrate` says so; close the
session and run `codegraphV migrate` again to finish. Running it again is always safe.

If `.codegraphV/` already exists, migration checks that it contains a healthy, completed
index before removing `.codegraph/`. An empty, damaged or unfinished destination keeps
both folders and prints recovery instructions. Resolve any reported failure and retry;
the original program and settings stay installed until the agents and indexes migrate
successfully, and each retry saves configuration backups in a separate folder.

### Copy-paste prompts for your agent

Paste one of these into Claude Code, Cursor, Codex or any agent that can run shell commands.
The agent does the work and reports back.

**Install**

```text
Install codegraphV (https://github.com/UntaDotMy/codegraphV) for me:
1. On Windows run in PowerShell: irm https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.ps1 | iex
   On macOS/Linux run: curl -fsSL https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.sh | sh
2. Run `codegraphV --version` (open a new shell if it is not found yet) and show me the output.
3. Run `codegraphV install --yes` to connect my coding agents, and show me which config files it changed.
4. In this project, run `codegraphV init` and then `codegraphV status`.
Tell me to restart my agent when you are done. Do not change anything else.
```

**Migrate from the original codegraph**

```text
Move me from the original codegraph to codegraphV (https://github.com/UntaDotMy/codegraphV):
1. If `codegraphV --version` fails, install it first. On Windows run in PowerShell:
   irm https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.ps1 | iex
   On macOS/Linux run: curl -fsSL https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.sh | sh
   (open a new shell if `codegraphV` is not found yet).
2. Run `codegraphV migrate --dry-run` and show me the plan.
3. Run `codegraphV migrate --yes` and show me its output.
Tell me to restart my agent, and that if it said something was in use, I should run
`codegraphV migrate` once more after the restart. Do not change anything else.
```

**Upgrade**

```text
Upgrade my codegraphV: run `codegraphV upgrade --check` and show me the result. If an update is
available, run `codegraphV upgrade`, then `codegraphV --version`, and tell me whether it says a
re-index is needed (if it does, run `codegraphV index` in this project). Tell me to restart my
agent afterwards. Do not change anything else.
```

**Uninstall**

```text
Uninstall codegraphV from this machine: run `codegraphV uninstall --yes` and show me its output
(it removes the codegraphV entries from my agents' config files and then removes the codegraphV
binary). Do not delete any .codegraphV/ folders unless I ask; if I want an index gone, it is
`codegraphV uninit` inside that project. Tell me to restart my agent afterwards.
```

### Upgrade and uninstall yourself

```bash
codegraphV upgrade              # latest release (re-runs the installer script, then refreshes agent config)
codegraphV upgrade v0.0.2       # a specific version
codegraphV upgrade --check      # only report whether an update exists

codegraphV uninstall            # remove agent config, then the codegraphV binary and its PATH entry
codegraphV uninstall --keep-cli # remove agent config only
codegraphV uninit               # inside a project: delete its .codegraphV/ index
```

While an MCP session runs, the server checks GitHub at most once a day for a newer release
and tells the agent when one exists. It never upgrades by itself. Turn the check off with
`CODEGRAPHV_NO_UPDATE_CHECK=1` (or `DO_NOT_TRACK=1`).

## Benchmarks

Every number below was produced by a script in this repository, and each section names the script
that reproduces it. Ground truth is always found from source text, never from either index. Where a
table is from an older release it says so — these figures are not re-measured on every release.

### Answering questions: Rust vs TypeScript vs ripgrep

[`scripts/bench/bench.py`](scripts/bench/bench.py) · measured 2026-10-26 with **v0.0.2**
([raw results](docs/benchmarks/rust-v0.0.2/report.md), [v0.0.1's](docs/benchmarks/rust-v0.0.1/report.md))

- **Machine:** Windows 11, AMD Ryzen 5 2600 (6 cores / 12 threads), 16 GB RAM.
- **Engines:** the Rust engine, the TypeScript engine it was ported from (v1.6.2), and a
  ripgrep-and-read baseline that models an agent without codegraph.
- **Inputs:** each engine indexes its own copy. "this repo" is pinned to commit `592b255e`.

| repo | engine | queries | recall | time | peak memory | est. output tokens |
|---|---|---|---|---|---|---|
| serde_json | **Rust** | 11 | **1.00** | **37 ms** | 12 MB | **696** |
| serde_json | TypeScript | 11 | 0.94 | 1039 ms | 190 MB | 3367 |
| serde_json | rg + read | 11 | 0.88 | 94 ms | 8 MB | 28970 |
| tokio | **Rust** | 11 | **0.92** | **48 ms** | 20 MB | **2433** |
| tokio | TypeScript | 11 | 0.80 | 1175 ms | 211 MB | 6239 |
| tokio | rg + read | 11 | 0.39 | 215 ms | 8 MB | 58016 |
| this repo | **Rust** | 12 | **1.00** | **53 ms** | 26 MB | **2632** |
| this repo | TypeScript | 12 | 0.74 | 1949 ms | 332 MB | 6235 |
| this repo | rg + read | 12 | 0.51 | 462 ms | 33 MB | 52987 |

- **recall:** the share of each query's target definitions whose source is in the answer.
- **est. output tokens:** response characters ÷ 4. A size estimate, not a provider tokenizer count
  and not whole-session usage.
- **time:** includes process start-up. Each CLI query is a new process, so most of the TypeScript
  time is Node start-up. A long-running MCP server pays that once.
- **Why recall differs:** both engines build the same graph, but the explore *renderer* is written to
  its own design rather than copied, so these columns compare renderers, not graphs.
- **The rg + read row varies between runs** (tokio was 0.64 in v0.0.1's run) because ripgrep searches
  in parallel and its capped output order is not fixed. A later `fix(bench)` pinned the sampled
  block; the v0.0.2 report predates that fix.

### Indexing a project from scratch

[`scripts/bench/bench.py`](scripts/bench/bench.py) · measured 2026-10-26 with **v0.0.2**

| repo | files | engine | wall time | CPU time | avg cores | peak memory |
|---|---|---|---|---|---|---|
| serde_json | 69 | **Rust** | **1.1 s** | 1.6 s | 1.4 | **68 MB** |
| serde_json | 69 | TypeScript | 4.1 s | 9.5 s | 2.3 | 480 MB |
| tokio | 555 | **Rust** | **5.2 s** | 7.9 s | 1.5 | **175 MB** |
| tokio | 555 | TypeScript | 15.3 s | 46.2 s | 3.0 | 1110 MB |
| codegraph (this repo) | 1878 | **Rust** | **30.9 s** | 120.5 s | 3.9 | **2126 MB** |
| codegraph (this repo) | 1878 | TypeScript | 51.0 s | 225.7 s | 4.4 | 3976 MB |

Both engines produced identical node and edge counts on every row (2100/6135, 9934/25384 and
49367/180304).

### Index speed between releases

[`scripts/bench/index_speed.py`](scripts/bench/index_speed.py) · median of 3 interleaved runs

- **v0.0.1 → v0.0.2:** on this repo, 54.7 s → 30.8 s with a byte-identical graph. v0.0.2 resolves
  references and runs synthesis on every core, and reads SQLite inside one transaction per unit of
  work instead of re-taking its lock per statement.

### Efficiency between releases

[`scripts/bench/efficiency_gate.py`](scripts/bench/efficiency_gate.py) · median of 3 interleaved runs

- **v0.0.2 → v0.0.3:** on this repo (pinned to `014ac5c0`, files over 1 MB skipped so both builds
  index the same files): 30.2 s vs 30.7 s, **88.7 vs 118.6 CPU-seconds (−25%)**, **1.8 vs 3.4 GB
  peak (−47%)**, identical graph. From not copying each file per `super` call, a JIT stack that
  starts at one page instead of 32 KB for each of thousands of compiled regexes, byte-limited
  resolver caches, and one resolve worker per physical core instead of per hardware thread.

### Memory on large inputs

[`scripts/bench/memory_budget_gate.py`](scripts/bench/memory_budget_gate.py)

- Several generated files up to the 10 MB limit index without blowing up memory.
- Parsing holds at most about 12 MB of source at a time. A graph of two million symbols (12
  generated 8 MB files) still peaks near 1.5 GB during the synthesis passes; those files parse one
  after the other. Lower `CODEGRAPHV_MAX_FILE_MB` to skip such files.

### Graph parity with the TypeScript engine

These are correctness gates, not speed measurements — did the Rust engine build the *same* graph?

| script | what it checks |
|---|---|
| [`scripts/bench/lang_parity.py`](scripts/bench/lang_parity.py) | Indexes one repo with both engines and diffs the graphs; exits non-zero on any difference. Run per language. |
| [`scripts/bench/verify_identical.py`](scripts/bench/verify_identical.py) | Indexes the bench repos fresh and diffs each against a baseline index. |
| [`scripts/bench/compare_index.py`](scripts/bench/compare_index.py) | Diffs two `codegraph.db` files as multisets of nodes, edges and unresolved refs. |
| [`scripts/bench/lang_probe.py`](scripts/bench/lang_probe.py) | One sample file per README language; fails if any yields no symbols. |

### Extraction and retrieval gates

| script | what it checks |
|---|---|
| [`scripts/rust-golden/verify.mjs`](scripts/rust-golden/verify.mjs) | Rust↔TS graph parity with a one-directional divergence class: a Rust file may *gain* rows from an unwrapped macro body, never lose or change one. |
| [`scripts/bench/gate_explore.py`](scripts/bench/gate_explore.py) | Gates a bench report's explore numbers: Rust recall and token use against TypeScript. |
| [`scripts/bench/gate_index.py`](scripts/bench/gate_index.py) | Gates a bench report's index numbers: Rust no slower than TS, CPU within budget. |
| [`scripts/bench/fts_rank.py`](scripts/bench/fts_rank.py) | Compares weighted BM25 ordering against FTS5's ranked iterator on an existing index. |
| [`scripts/bench/autosync_e2e.py`](scripts/bench/autosync_e2e.py) | End-to-end: the MCP server keeps the graph in sync with file edits. |

### Agent A/B — does an agent actually do better?

[`scripts/agent-eval/`](scripts/agent-eval/) · the harness that measures retrieval *from the agent's
side*, rather than from the CLI

Deterministic probes measure what a command prints. They cannot measure whether that output let an
agent stop reading. These harnesses run a real agent twice — with and without codegraphV, or against
two builds — and score the transcript. Entry point:
[`docs/benchmarks/agent-eval-feedback-metrics.md`](docs/benchmarks/agent-eval-feedback-metrics.md).

| metric | the question it answers |
|---|---|
| **Residual context occupancy** | How many tokens the arm's retrieval still occupies when the run ends — the headroom every following turn works in. |
| **Explore sufficiency** | Was the response *enough*? Read from what the agent did next: explored again, read a file, or answered. |
| **Explore allocation** | What share of the returned bytes did the final answer actually cite. |

| harness | compares |
|---|---|
| [`ab-new-vs-baseline.sh`](scripts/agent-eval/ab-new-vs-baseline.sh) | Two codegraphV builds on one question (the main A/B). |
| [`ab-rust-builds.sh`](scripts/agent-eval/ab-rust-builds.sh) | The same, for two Rust binaries — each arm indexes with *its own* binary and serves it as the MCP server, with a daemon pre-warmed per arm. |
| [`run-all.sh`](scripts/agent-eval/run-all.sh) | with codegraphV vs without. |
| [`ab-sufficiency.sh`](scripts/agent-eval/ab-sufficiency.sh), [`ab-impl.sh`](scripts/agent-eval/ab-impl.sh), [`ab-adoption.sh`](scripts/agent-eval/ab-adoption.sh), [`ab-hook.sh`](scripts/agent-eval/ab-hook.sh) | Purpose-built arms (sufficiency, implementation, adoption, hook). |

`parse-run.mjs` scores a transcript; `compare-arms.mjs` prints the side-by-side table. Both have a
`--selftest` that must stay green.

### Audit reports

| date | baseline → release | what it covers |
|---|---|---|
| [2026-10-30](docs/benchmarks/audit-2026-10-30/report.md) | `c645adc0` | Reliability: SQLite WAL fix, Windows installer recovery, migration safety, query cost. |
| [2026-11-03](docs/benchmarks/audit-2026-11-03/report.md) | v0.0.8 → v0.0.9 | All 16 crates, retrieval, search, graph accuracy, components, protocol recovery, locking. Its [upstream ledger](docs/benchmarks/audit-2026-11-03/upstream.json) marks every reviewed upstream commit ported, partial, native, reference-only or pending. |

The audit is explicit that it does not certify every function, and the broad parity checkpoint is
deliberately not advanced on its own.

### Where Rust is not better

- **Indexing a very large repo uses more memory than v0.0.1 did.** On this repo (2,053 files) the
  peak with v0.0.12 is about 1.6 GB, against 1.1 GB for v0.0.1 (and 4.0 GB for TypeScript). Lower
  `CODEGRAPHV_RESOLVE_WORKERS` and `CODEGRAPHV_PARSE_WORKERS` to trade speed for memory.
- **The binary is large:** about 77 MB on Windows, because tree-sitter grammars for every language
  are compiled in, plus a WebAssembly runtime for some grammars. Shrinking it is open work.
- **Fewer surfaces than the original:** no npm package and no JavaScript library API; the browser
  viewer (`codegraphV ui`) is a preview behind `CODEGRAPHV_UI=1` and does not have every view the
  original has; `codegraphV init` does not yet offer to index the other repositories it finds under
  the folder.
- **This is an early release.** Parity was checked on serde_json, tokio, this repo and one public
  repo for each of 15 languages, plus the workspace test suite. That is a lot of code, but not every
  repo in the world: if the two engines disagree on your code, that is a bug — please open an issue
  with the repo.

## How it stays in sync

- **During a session:** the MCP server watches the project (native file events on Windows,
  macOS and Linux) and re-indexes the files you change.
- **Between sessions:** when a session starts, it first catches up on edits made while no
  session was running, so the first answer is already current.
- **Several sessions on one project:** they share one background server per project
  (`codegraphV daemon` shows or stops it). It exits on its own once no session uses it.
- **When the agent goes away:** its server notices within about 5 seconds and exits, even if
  the agent was killed or something else still holds its input open.
- **Turning it off:** set `CODEGRAPHV_NO_WATCH=1`, then run `codegraphV sync` by hand.
- **The CLI:** `codegraphV explore`, `node` and `files` answer from the index as it is, as
  the original's CLI does. While an agent session runs, its server keeps the index current;
  otherwise run `codegraphV sync` after editing.

## CLI

```
codegraphV init [path]          index a project            codegraphV status      index status
codegraphV index / sync         rebuild / reconcile        codegraphV explore "<symbols or question>"
codegraphV query <name>         search symbols             codegraphV node <symbol>
codegraphV callers|callees <s>  relations                  codegraphV impact <symbol>
codegraphV affected <files…>    files affected by a change codegraphV context "<task>"
codegraphV implementations <t>  types implementing t       codegraphV cycles [path]   circular imports (exit 1)
codegraphV install / uninstall  agent config               codegraphV upgrade [version] [--check]
codegraphV serve --mcp          the MCP server agents run  codegraphV daemon [--stop]
codegraphV migrate              move over from the original codegraph
```

`codegraphV <command> --help` lists each command's options.

## Getting useful answers from your agent

Start with `codegraphV_explore` and name the symbols or files you need. For a call path,
name both ends in one query, such as `loginUser createSession`. Include a file path when
several definitions share a name. Returned source has line numbers and can be used to edit.

When source does not fit, follow the returned continuation arguments. Exact ranges such
as `src/auth.ts:40-90` return those lines even if they were shown earlier. Oversized lines
come with character pages and a source hash that detects edits between pages.

| Symptom | Next step |
|---|---|
| The agent cannot see `codegraphV_explore` after installation | Restart the agent once and check its MCP server status; `codegraphV install --print-config codex` shows the expected Codex configuration (replace `codex` with your agent's target name). |
| The server has no default project, or you are working in a monorepo | Pass `projectPath` for the indexed project; indexing a different project is a separate choice. |
| A recent CLI answer is stale | Run `codegraphV sync` in that project; the MCP server follows edits while a session is running. |
| A query has no matches | Use an exact symbol name or indexed file path; matching is lexical, and does not require an embedding service. |
| Migration reports a damaged or unfinished destination | Keep both folders, move the destination aside as directed, then rerun `codegraphV migrate`. |

## Configuration

| variable | effect |
|---|---|
| `CODEGRAPHV_NO_UPDATE_CHECK=1` / `DO_NOT_TRACK=1` | no daily release check |
| `CODEGRAPHV_NO_PROMPT_HOOK=1` | the Claude prompt hook prints nothing |
| `CODEGRAPHV_GUARD=0` | the Claude search guard allows everything |
| `CODEGRAPHV_NO_WATCH=1` | no file watching in MCP sessions |
| `CODEGRAPHV_DAEMON=0` | each session runs its own server instead of the shared one |
| `CODEGRAPHV_PPID_POLL_MS` | how often (ms) a server checks that its agent is still there (default 5000, `0` = never) |
| `CODEGRAPHV_DIR` | the per-project folder name (default `.codegraphV`) |
| `CODEGRAPHV_PARSE_WORKERS` / `CODEGRAPHV_RESOLVE_WORKERS` | thread counts for indexing (default: parsing uses every hardware thread, resolving one per physical core) |
| `CODEGRAPHV_MAX_FILE_MB` | largest source file indexed, in MB (default 10, `0` = no limit) |
| `CODEGRAPHV_PARSE_TIMEOUT_MS` | how long one file may parse before it is recorded as a parse error, in ms (default 10000, plus as much again per MiB of source; `0` = no limit) |
| `CODEGRAPHV_RESOLVER_CACHE_MB` | file text each resolve worker may cache, in MB (default 8) |
| `CODEGRAPHV_INSTALL_DIR`, `CODEGRAPHV_VERSION` | install-script / upgrade location and version |

## Privacy

codegraphV sends nothing anywhere. The only network request is the daily check for a new
release (a request to github.com, which you can turn off). The TypeScript original has
opt-out anonymous usage statistics. This rewrite removed them entirely: there is no telemetry
code in the binary.

## Building from source

```bash
git clone -b rust-rewrite https://github.com/UntaDotMy/codegraphV
cd codegraphV
cargo build --release -p codegraph-cli    # target/release/codegraphV(.exe)
cargo test --workspace --release
```

The TypeScript engine stays in `src/` as the parity reference. `scripts/bench/lang_parity.py`
diffs the two graphs on any repo, `scripts/bench/bench.py` reproduces the tables above, and
`scripts/check_names.py` checks that no user-visible text still uses the original's names.

## License

MIT, the same as the original. See [LICENSE](LICENSE): Copyright (c) 2026 Colby McHenry. The
Rust port is distributed under the same license, with the original copyright notice kept.

<div align="center">

Original project: **[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph)** by Colby McHenry · Rust rewrite: [UntaDotMy/codegraphV](https://github.com/UntaDotMy/codegraphV) · [Report a bug](https://github.com/UntaDotMy/codegraphV/issues)

</div>
