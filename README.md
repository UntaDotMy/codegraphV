<div align="center">

# CodeGraph — Rust rewrite

**A full rewrite in Rust of [CodeGraph](https://github.com/colbymchenry/codegraph) by [Colby McHenry](https://github.com/colbymchenry).**
One native executable: indexer, CLI and MCP server. There is no Node.js and no npm, and it has no telemetry.

`v0.0.1` · Windows · macOS · Linux · MIT

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

## What it does

CodeGraph parses your code with tree-sitter and stores every symbol and every relation
between symbols (calls, imports, extends, implements, routes and more) in a local SQLite
database in `.codegraph/`. AI coding agents (Claude Code, Cursor, Codex, opencode and others)
query it over MCP, so a question like "how does a login request reach the database?" is
answered by one `codegraph_explore` call. Without it, the agent loops over grep and file reads.

Everything runs on your machine. The rewrite keeps:

- **The same MCP tools:** `codegraph_explore`, `codegraph_node`, `codegraph_search`,
  `codegraph_callers`, `codegraph_callees`, `codegraph_impact`, `codegraph_files` and
  `codegraph_status`.
- **The same languages:** TypeScript, JavaScript, Python, Go, Rust, Java, C#, PHP, Ruby, C,
  C++, Objective-C, Swift, Kotlin, Scala, Dart, Svelte, Vue, Astro, Liquid, Pascal/Delphi,
  Lua, Luau, R, CFML, COBOL, VB.NET, Erlang, Solidity, Terraform, Nix, ArkTS and more.
- **The same framework resolvers:** Express, Next.js, Django, Rails, Laravel, Spring, FastAPI,
  React Native, SvelteKit, Nuxt and the rest.
- **The same dynamic-dispatch bridges** (callbacks, events, React re-renders).
- **The same agent installer** for Claude Code, Cursor, Codex, opencode, Gemini, Kiro,
  Hermes, Antigravity and GitHub Copilot.
- **Automatic sync:** the index follows your edits while an agent session runs, and catches
  up on edits made while no session was running.
- **The Claude Code prompt hook,** which adds CodeGraph context to "how / where / trace"
  prompts.
- **The update notice and `codegraph upgrade`.**

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

- **Windows:** the binary goes in `%LOCALAPPDATA%\codegraph\bin`, which is added to your user
  PATH.
- **macOS / Linux:** the binary goes in `~/.local/bin`.
- **Options:** set `CODEGRAPH_VERSION=v0.0.1` to pin a version, or `CODEGRAPH_INSTALL_DIR` to
  pick the folder.

Then connect your agents and index a project:

```bash
codegraph install          # detects your agents and writes their MCP config (asks first)
cd your-project
codegraph init             # builds .codegraph/ for this project
```

Restart the agent once after `codegraph install`. New projects only need `codegraph init`.

**Coming from the npm version?** Remove it first so the old `codegraph` does not shadow this
one on PATH: `npm rm -g @colbymchenry/codegraph`. Then run `codegraph install --refresh` to
update your agents' config for this binary. Existing `.codegraph/` indexes are read as they
are.

### Copy-paste prompts for your agent

Paste one of these into Claude Code, Cursor, Codex or any agent that can run shell commands.
The agent does the work and reports back.

**Install**

```text
Install the Rust CodeGraph (https://github.com/UntaDotMy/codegraphV) for me:
1. If `npm ls -g @colbymchenry/codegraph` shows the npm version, run `npm rm -g @colbymchenry/codegraph`.
2. On Windows run in PowerShell: irm https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.ps1 | iex
   On macOS/Linux run: curl -fsSL https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.sh | sh
3. Run `codegraph --version` (open a new shell if it is not found yet) and show me the output.
4. Run `codegraph install --yes` to connect my coding agents, and show me which config files it changed.
5. In this project, run `codegraph init` and then `codegraph status`.
Tell me to restart my agent when you are done. Do not change anything else.
```

**Upgrade**

```text
Upgrade my CodeGraph: run `codegraph upgrade --check` and show me the result. If an update is
available, run `codegraph upgrade`, then `codegraph --version`, and tell me whether it says a
re-index is needed (if it does, run `codegraph index` in this project). Tell me to restart my
agent afterwards. Do not change anything else.
```

**Uninstall**

```text
Uninstall CodeGraph from this machine: run `codegraph uninstall --yes` and show me its output
(it removes the CodeGraph entries from my agents' config files and then removes the codegraph
binary). Do not delete any .codegraph/ folders unless I ask; if I want an index gone, it is
`codegraph uninit` inside that project. Tell me to restart my agent afterwards.
```

### Upgrade and uninstall yourself

```bash
codegraph upgrade              # latest release (re-runs the installer script, then refreshes agent config)
codegraph upgrade v0.0.2       # a specific version
codegraph upgrade --check      # only report whether an update exists

codegraph uninstall            # remove agent config, then the codegraph binary and its PATH entry
codegraph uninstall --keep-cli # remove agent config only
codegraph uninit               # inside a project: delete its .codegraph/ index
```

While an MCP session runs, the server checks GitHub at most once a day for a newer release
and tells the agent when one exists. It never upgrades by itself. Turn the check off with
`CODEGRAPH_NO_UPDATE_CHECK=1` (or `DO_NOT_TRACK=1`).

## Benchmarks, measured honestly

These results come from [`scripts/bench/bench.py`](scripts/bench/bench.py), run on 2026-10-25.

- **Machine:** Windows 11, AMD Ryzen 5 2600 (6 cores / 12 threads), 16 GB RAM.
- **Engines:** the Rust engine (this release) against the TypeScript engine it was ported
  from (v1.6.2, built from this repo's `src/`), plus a ripgrep-and-read baseline that models
  an agent without CodeGraph.
- **Inputs:** each engine indexes its own copy of each repo.
- **Ground truth:** found from source text alone, never from either index.
- **Raw results:** [`docs/benchmarks/rust-v0.0.1/`](docs/benchmarks/rust-v0.0.1/report.md).

### Answering questions (`codegraph explore`, median per query)

| repo | engine | queries | recall | time | peak memory | output tokens |
|---|---|---|---|---|---|---|
| serde_json | **Rust** | 11 | **1.00** | **37 ms** | 10 MB | **696** |
| serde_json | TypeScript | 11 | 0.94 | 1102 ms | 190 MB | 3367 |
| serde_json | rg + read | 11 | 0.88 | 104 ms | 8 MB | 28970 |
| tokio | **Rust** | 11 | **0.92** | **50 ms** | 18 MB | **2433** |
| tokio | TypeScript | 11 | 0.80 | 1217 ms | 211 MB | 6239 |
| tokio | rg + read | 11 | 0.64 | 218 ms | 8 MB | 63735 |
| codegraph (this repo) | **Rust** | 12 | **1.00** | **57 ms** | 24 MB | **2376** |
| codegraph (this repo) | TypeScript | 12 | 0.74 | 1976 ms | 328 MB | 6236 |
| codegraph (this repo) | rg + read | 12 | 0.71 | 475 ms | 32 MB | 63150 |

How to read the columns:

- **recall:** the share of each query's target definitions whose source is in the answer.
- **time:** includes process start-up. Each CLI query is a new process, so most of the
  TypeScript time is Node start-up and loading. A long-running MCP server pays that cost only
  once.
- **Why recall differs:** the two engines build the same graph, but the explore output is not
  the same. The Rust explore renderer was written to its own design rather than copied, so
  the recall and token columns compare the two renderers, not the graphs.

### Indexing (`codegraph init`, a full index from scratch)

| repo | files | engine | wall time | CPU time | avg cores | peak memory |
|---|---|---|---|---|---|---|
| serde_json | 69 | **Rust** | **1.9 s** | 2.1 s | 1.1 | **53 MB** |
| serde_json | 69 | TypeScript | 4.7 s | 10.0 s | 2.1 | 486 MB |
| tokio | 555 | **Rust** | **9.1 s** | 11.3 s | 1.2 | **156 MB** |
| tokio | 555 | TypeScript | 16.3 s | 47.8 s | 2.9 | 1120 MB |
| codegraph (this repo) | 1869 | Rust | 54.9 s | **118 s** | 2.2 | **1144 MB** |
| codegraph (this repo) | 1869 | **TypeScript** | **52.4 s** | 230 s | 4.4 | 3928 MB |

Both engines produced identical node and edge counts on every row (2100/6135, 9934/25384 and
49190/179836).

### Where Rust is not better

- **Indexing a large repo is not faster in wall time.** On this repo (1869 files, 180k edges)
  the Rust engine took 54.9 s against TypeScript's 52.4 s, about 5% slower. It used half the
  CPU time and less than a third of the memory, but it kept fewer cores busy (2.2 against
  4.4). Spreading that work over more cores is open work. On the small and medium repos
  Rust indexed 1.8–2.6× faster.
- **The binary is large:** about 74 MB, because tree-sitter grammars for every language are
  compiled in, plus a WebAssembly runtime that loads the original's grammar files for some
  languages. The npm package downloads a full Node.js runtime instead, but that is no
  excuse: shrinking the binary is open work.
- **Fewer surfaces than the original:**
  - no npm package and no JavaScript library API;
  - the browser viewer (`codegraph ui`) is a preview behind `CODEGRAPH_UI=1`, as it is
    upstream, and does not have every view the original has;
  - `codegraph init` does not yet offer to index the other repositories it finds under the
    folder.
- **Version 0.0.1 is a first release.** Parity was checked as follows:
  - a graph diff against the TypeScript engine on serde_json, tokio, this repo, and one
    public repo for each of 15 languages;
  - the workspace test suite.

  That is a lot of code, but not every repo in the world. If the Rust and TypeScript engines
  disagree on your code, that is a bug: please open an issue with the repo.

## How it stays in sync

- **During a session:** the MCP server watches the project (native file events on Windows,
  macOS and Linux) and re-indexes the files you change.
- **Between sessions:** when a session starts, it first catches up on edits made while no
  session was running, so the first answer is already current.
- **Several sessions on one project:** they share one background server per project
  (`codegraph daemon` shows or stops it).
- **Turning it off:** set `CODEGRAPH_NO_WATCH=1`, then run `codegraph sync` by hand.

## CLI

```
codegraph init [path]          index a project            codegraph status      index status
codegraph index / sync         rebuild / reconcile        codegraph explore "<symbols or question>"
codegraph query <name>         search symbols             codegraph node <symbol>
codegraph callers|callees <s>  relations                  codegraph impact <symbol>
codegraph affected <files…>    files affected by a change codegraph context "<task>"
codegraph install / uninstall  agent config               codegraph upgrade [version] [--check]
codegraph serve --mcp          the MCP server agents run  codegraph daemon [--stop]
```

`codegraph <command> --help` lists each command's options.

## Configuration

| variable | effect |
|---|---|
| `CODEGRAPH_NO_UPDATE_CHECK=1` / `DO_NOT_TRACK=1` | no daily release check |
| `CODEGRAPH_NO_PROMPT_HOOK=1` | the Claude prompt hook prints nothing |
| `CODEGRAPH_NO_WATCH=1` | no file watching in MCP sessions |
| `CODEGRAPH_DAEMON=0` | each session runs its own server instead of the shared one |
| `CODEGRAPH_PARSE_WORKERS` / `CODEGRAPH_RESOLVE_WORKERS` | thread counts for indexing |
| `CODEGRAPH_INSTALL_DIR`, `CODEGRAPH_VERSION` | install-script / upgrade location and version |

## Privacy

CodeGraph sends nothing anywhere. The only network request is the daily check for a new
release (a request to github.com, which you can turn off). The TypeScript original has
opt-out anonymous usage statistics. This rewrite removed them entirely: there is no telemetry
code in the binary.

## Building from source

```bash
git clone -b rust-rewrite https://github.com/UntaDotMy/codegraphV
cd codegraphV
cargo build --release -p codegraph-cli    # target/release/codegraph(.exe)
cargo test --workspace --release
```

The TypeScript engine stays in `src/` as the parity reference. `scripts/bench/lang_parity.py`
diffs the two graphs on any repo, and `scripts/bench/bench.py` reproduces the tables above.

## License

MIT, the same as the original. See [LICENSE](LICENSE): Copyright (c) 2026 Colby McHenry. The
Rust port is distributed under the same license, with the original copyright notice kept.

<div align="center">

Original project: **[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph)** by Colby McHenry · Rust rewrite: [UntaDotMy/codegraphV](https://github.com/UntaDotMy/codegraphV) · [Report a bug](https://github.com/UntaDotMy/codegraphV/issues)

</div>
