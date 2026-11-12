use clap::{Args, Parser, Subcommand};
mod implementations;
mod migrate;
mod prompt_hook;
mod remove_cli;
mod upgrade;
mod wire;

// Indexing allocates heavily on every core. The system allocator on Windows returns large
// blocks to the kernel and takes a process-wide lock for them, so resolve stops scaling
// with threads; mimalloc keeps memory per thread (DEV-87).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use codegraph_core::{directory, CodeGraph, IndexOptions, OpenOptions};
use codegraph_mcp::{render, tools::ToolHandler};
use codegraph_types::*;
use serde_json::{json, Value};
use std::{
    io::{self, IsTerminal, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Parser)]
#[command(
    name = "codegraphV",
    version,
    about = "Local code intelligence — native Rust executable"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Args, Clone)]
struct Project {
    #[arg(short, long)]
    path: Option<String>,
}
#[derive(Args)]
struct Symbol {
    symbol: String,
    #[command(flatten)]
    project: Project,
    #[arg(short, long)]
    file: Option<String>,
    #[arg(short, long, default_value_t = 20)]
    limit: usize,
    #[arg(short, long)]
    json: bool,
}
#[derive(Subcommand)]
enum Command {
    /// Initialize and index a project.
    Init {
        #[arg(default_value = ".")]
        path: String,
        #[arg(short, long)]
        force: bool,
        #[arg(short, long)]
        verbose: bool,
        /// Accepted for compatibility; `init` never prompts.
        #[arg(short, long)]
        yes: bool,
        #[arg(short, long, hide = true)]
        index: bool,
        /// Install CodeGraphV's Git sync hooks (post-commit, post-merge, post-checkout).
        #[arg(long)]
        git_hooks: bool,
        /// Keep .codegraphV/ out of git entirely: its .gitignore ignores itself too. Also converts
        /// an initialized project, without indexing again.
        #[arg(long)]
        local_only: bool,
    },
    /// Rebuild the code graph.
    Index {
        #[arg(default_value = ".")]
        path: String,
        #[arg(short, long)]
        force: bool,
        #[arg(short, long)]
        quiet: bool,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Reconcile changed files with the index.
    Sync {
        #[arg(default_value = ".")]
        path: String,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Remove this project's index. Agent configuration is removed separately by uninstall.
    Uninit {
        #[arg(default_value = ".")]
        path: String,
        #[arg(short, long)]
        force: bool,
    },
    /// Show index status.
    Status {
        #[arg(default_value = ".")]
        path: String,
        #[arg(short, long)]
        json: bool,
        /// Exit 1 unless the index is finished, up to date, and every language that should yield
        /// symbols did.
        #[arg(long)]
        strict: bool,
    },
    /// Search indexed symbols.
    Query {
        search: String,
        #[command(flatten)]
        project: Project,
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
        #[arg(short, long)]
        kind: Option<String>,
        #[arg(short, long)]
        json: bool,
    },
    /// Find flows and read relevant source.
    Explore {
        #[arg(required=true,num_args=1..)]
        query: Vec<String>,
        #[command(flatten)]
        project: Project,
        #[arg(long)]
        max_files: Option<usize>,
        /// Unicode character column for a single-line source fragment.
        #[arg(long)]
        column: Option<usize>,
        /// Content hash from the previous source fragment.
        #[arg(long, requires = "column")]
        source_hash: Option<String>,
    },
    /// Read a symbol or indexed file.
    Node {
        name: Option<String>,
        #[command(flatten)]
        project: Project,
        #[arg(short, long)]
        file: Option<String>,
        #[arg(long)]
        offset: Option<usize>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        symbols_only: bool,
    },
    /// List tracked files.
    Files {
        #[command(flatten)]
        project: Project,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        pattern: Option<String>,
        /// Show only test files.
        #[arg(long, conflicts_with = "no_tests")]
        tests: bool,
        /// Exclude test files.
        #[arg(long)]
        no_tests: bool,
        #[arg(long,default_value="tree",value_parser=["tree","flat","grouped","package"])]
        format: String,
        #[arg(long)]
        max_depth: Option<usize>,
        #[arg(long)]
        no_metadata: bool,
        #[arg(short, long)]
        json: bool,
    },
    Callers(Symbol),
    Callees(Symbol),
    /// Show the symbols affected by a change.
    Impact {
        symbol: String,
        #[command(flatten)]
        project: Project,
        #[arg(short, long)]
        file: Option<String>,
        #[arg(short, long, default_value_t = 2)]
        depth: usize,
        #[arg(short, long)]
        json: bool,
    },
    /// List the types that implement or extend a trait, interface or class.
    Implementations {
        symbol: String,
        #[command(flatten)]
        project: Project,
        /// Narrow definitions by file path or suffix (no match: show all with a note).
        #[arg(short, long)]
        file: Option<String>,
        /// Levels of subtypes to list (1: direct implementors only).
        #[arg(short, long, default_value_t = 1)]
        depth: u32,
        #[arg(short, long)]
        json: bool,
    },
    /// Show files affected by changes to source files.
    Affected {
        files: Vec<String>,
        #[command(flatten)]
        project: Project,
        #[arg(long)]
        stdin: bool,
        #[arg(short, long, default_value_t = 5)]
        depth: u32,
        #[arg(short, long)]
        filter: Option<String>,
        #[arg(short, long)]
        json: bool,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Build task context from the graph.
    Context {
        #[arg(required=true,num_args=1..)]
        task: Vec<String>,
        #[command(flatten)]
        project: Project,
        #[arg(short,long,default_value="markdown",value_parser=["markdown","json"])]
        format: String,
        #[arg(short = 'n', long)]
        max_nodes: Option<usize>,
        #[arg(long)]
        no_code: bool,
    },
    #[command(hide = true)]
    Serve {
        #[command(flatten)]
        project: Project,
        #[arg(long)]
        mcp: bool,
        #[arg(long)]
        no_watch: bool,
        /// Accepted for host compatibility; codegraphV has no telemetry to disable.
        #[arg(long)]
        no_telemetry: bool,
    },
    #[command(name = "__daemon", hide = true)]
    DaemonServer {
        #[arg(long)]
        path: String,
        #[arg(long, default_value_t = 600)]
        idle_timeout: u64,
    },
    /// Show or stop the current project's native background server.
    #[command(alias = "daemons")]
    Daemon {
        #[command(flatten)]
        project: Project,
        #[arg(long)]
        stop: bool,
        #[arg(short, long)]
        json: bool,
    },
    /// Remove a stale database lock after its owner has exited.
    Unlock {
        #[arg(default_value = ".")]
        path: String,
    },
    /// Open the local browser viewer (preview).
    #[command(alias="web",hide=std::env::var("CODEGRAPHV_UI").as_deref()!=Ok("1"))]
    Ui {
        #[arg(default_value = ".")]
        path: String,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        no_open: bool,
        #[arg(long)]
        read_only: bool,
    },
    /// Connect codegraphV to coding agents.
    Install {
        #[arg(short, long, default_value = "auto")]
        target: String,
        #[arg(short,long,value_parser=["global","local"])]
        location: Option<String>,
        #[arg(short, long)]
        yes: bool,
        #[arg(short, long)]
        init: bool,
        #[arg(long)]
        no_permissions: bool,
        /// Claude prompt hook choice; refresh preserves the existing choice by default.
        #[arg(long, value_parser=["preserve", "on", "off"])]
        prompt_hook: Option<String>,
        #[arg(long)]
        print_config: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
    /// Remove codegraphV configuration from coding agents.
    Uninstall {
        #[arg(short, long, default_value = "all")]
        target: String,
        #[arg(short,long,value_parser=["global","local"])]
        location: Option<String>,
        #[arg(short, long)]
        yes: bool,
        #[arg(long)]
        keep_cli: bool,
    },
    /// Claude UserPromptSubmit hook: inject codegraphV context for structural prompts (reads
    /// {prompt,cwd} JSON on stdin).
    #[command(hide = true)]
    PromptHook,
    /// Update codegraphV to the latest release, or to the version given.
    Upgrade {
        /// Release to install, for example v0.0.2 (default: the latest)
        version: Option<String>,
        /// Show current and latest versions without changing anything
        #[arg(long)]
        check: bool,
        /// Reinstall even when already on that version
        #[arg(long)]
        force: bool,
    },
    /// Move from the original codegraph to codegraphV: agent configuration, project indexes
    /// (kept, not rebuilt) and the original's install.
    Migrate {
        /// Show what would change and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask before changing anything
        #[arg(short, long)]
        yes: bool,
        /// Another project folder whose .codegraph/ index to move (repeatable)
        #[arg(long = "path", value_name = "DIR")]
        paths: Vec<std::path::PathBuf>,
    },
    /// Print the installed version.
    Version,
}
fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}
fn root(path: Option<&str>) -> Result<String> {
    let path = path.unwrap_or(".");
    let absolute = nodepath::resolve(&[path]);
    directory::find_nearest_code_graph_root(&absolute).ok_or_else(|| {
        format!("Project is not indexed: {absolute}. Run `codegraphV init` first.").into()
    })
}
fn open(project: &Project) -> Result<CodeGraph> {
    Ok(CodeGraph::open(
        &root(project.path.as_deref())?,
        OpenOptions::default(),
    )?)
}
/// Said on stderr by a read command that finds another process rebuilding the index, so a `--json`
/// answer stays parseable.
const REBUILD_NOTE: &str = "A rebuild of this index is in progress (another codegraphV process is indexing); answers may be incomplete until it finishes.";

/// Open the index for a command that only reads it. Migrations, bulk-load repair and WAL
/// maintenance are the writer's: a read-write open during a full index recreates the indexes the
/// indexer dropped for its bulk load, and the indexer's next write then fails with "database is
/// locked" (upstream 01873666, 475e56b2). `announce` says on stderr when another process is rebuilding.
fn open_read_only(project: &Project, announce: bool) -> Result<CodeGraph> {
    let cg = CodeGraph::open(&root(project.path.as_deref())?, OpenOptions { read_only: true, ..OpenOptions::default() })?;
    if announce && cg.is_rebuild_in_progress() {
        eprintln!("{REBUILD_NOTE}");
    }
    Ok(cg)
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn tool(project: Project, name: &str, args: Value) -> Result<()> {
    let path = root(project.path.as_deref())?;
    let mut handler = ToolHandler::new(Some(path), false);
    handler.catch_up = false; // one-shot: answer from the index as it is (BUG-99)
    handler.read_only = true;
    let result = handler.call(name, &args);
    if handler.rebuild_in_progress() {
        eprintln!("{REBUILD_NOTE}");
    }
    if result.is_error {
        return Err(result.message().into());
    }
    println!("{}", result.message());
    Ok(())
}
/// `n` with thousands separators, as `status` prints counts.
fn format_number(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Up to three of `paths`, and how many more there are.
fn sample_paths(paths: &[String]) -> String {
    let shown = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > 3 { format!("{shown} (+{} more)", format_number(paths.len() as u64 - 3)) } else { shown }
}

/// `status`'s files-by-language rows, each with how many files yielded symbols when that isn't all
/// of them, and a warning for a language where none did (upstream 7889da66).
fn print_language_coverage(cg: &CodeGraph, coverage: &codegraph_core::codegraph::LanguageCoverage) -> Result<()> {
    let mut rows: Vec<(String, u64)> = cg
        .get_stats()?
        .files_by_language
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(l, n)| (l.as_str().to_string(), *n))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\nFiles by Language:");
    for (language, count) in &rows {
        let with_symbols = match coverage.parsed_files_by_language.get(language) {
            Some(parsed) if parsed != count => format!("  ({} with symbols)", format_number(*parsed)),
            _ => String::new(),
        };
        println!("  {language:<15} {}{with_symbols}", format_number(*count));
    }
    // Worded for both causes: a lone config stub with nothing to extract reads exactly like a
    // parser that isn't working, and the file names are what tell them apart.
    if !coverage.unparsed_languages.is_empty() {
        let files = cg.get_files()?;
        println!();
        for language in &coverage.unparsed_languages {
            let paths: Vec<String> = files.iter().filter(|f| f.language.as_str() == language).map(|f| f.path.clone()).collect();
            println!("Warning: No symbols came from any {language} file — fine if they hold no code, otherwise its parser isn't working: {}", sample_paths(&paths));
        }
    }
    Ok(())
}

fn index_project(path: &str, force: bool, verbose: bool, quiet: bool) -> Result<()> {
    let root = nodepath::resolve(&[path]);
    if let Some(reason) = codegraph_core::utils::validate_project_path(&root) {
        return Err(reason.into());
    }
    if !force {
        if let Some(reason) = directory::unsafe_index_root_reason(&root) {
            return Err(format!("{reason}; pass --force to index this directory.").into());
        }
    }
    let mut cg = if directory::is_initialized(&root) {
        CodeGraph::open(&root, OpenOptions::default())?
    } else {
        CodeGraph::init(&root)?
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::SeqCst);
    })?;
    let result = cg.index_all(
        &IndexOptions {
            verbose,
            ..Default::default()
        },
        None,
        Some(&cancelled),
    )?;
    if !quiet {
        print_json(&result)?;
    }
    cg.close();
    if !result.success {
        return Err("Indexing did not complete; see the errors above.".into());
    }
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = match cli.command {
        Some(command) => command,
        None if io::stdin().is_terminal() => Command::Install {
            target: "auto".into(),
            location: None,
            yes: false,
            init: false,
            no_permissions: false,
            prompt_hook: None,
            print_config: None,
            refresh: false,
        },
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            return Ok(());
        }
    };
    match command {
        Command::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        Command::Migrate { dry_run, yes, paths } => {
            let code = migrate::run(migrate::Options { dry_run, yes, paths })?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::PromptHook => prompt_hook::run(),
        Command::Upgrade { version, check, force } => {
            let code = upgrade::run(version, check, force);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::Init {
            path,
            force,
            verbose,
            git_hooks,
            local_only,
            ..
        } => {
            let root = nodepath::resolve(&[&path]);
            // `--local-only` on an initialized project only converts the file (upstream fb71a9f4).
            if !(local_only && directory::is_initialized(&root)) {
                index_project(&path, force, verbose, false)?;
            }
            if local_only {
                let file = format!("{}/.gitignore", std::path::Path::new(&directory::get_code_graph_dir(&root)).file_name().unwrap_or_default().to_string_lossy());
                match directory::make_gitignore_local_only(&root)? {
                    directory::LocalOnlyGitignore::Written => println!("{file} now ignores itself, so nothing of CodeGraphV's shows in git."),
                    directory::LocalOnlyGitignore::Unchanged => println!("{file} already ignores itself."),
                    directory::LocalOnlyGitignore::Customized => println!("Left {file} as it is because it has your own rules. A lone * in it would keep the whole folder out of git."),
                    directory::LocalOnlyGitignore::Tracked => println!("Left {file} as it is because it is committed to this repository, so changing it would show up in git."),
                }
            }
            if git_hooks {
                let root = nodepath::resolve(&[&path]);
                let result = codegraph_sync::git_hooks::install_git_sync_hook(&root, None)?;
                if let Some(reason) = &result.skipped {
                    println!("Git sync hooks skipped: {reason}");
                } else {
                    let names: Vec<_> = result.installed.iter().map(|h| h.as_str()).collect();
                    println!("Installed Git sync hooks: {}", names.join(", "));
                    if let Some(unsupported) = &result.unsupported {
                        let names: Vec<_> = unsupported.iter().map(|h| h.as_str()).collect();
                        println!("Kept existing non-shell hooks unchanged: {}", names.join(", "));
                    }
                }
            }
        }
        Command::Index {
            path,
            force,
            quiet,
            verbose,
        } => index_project(&path, force, verbose, quiet)?,
        Command::Sync { path, quiet } => {
            let mut cg = open(&Project { path: Some(path) })?;
            let result = cg.sync(&IndexOptions::default(), None)?;
            if !quiet {
                print_json(&result)?;
                // Sync re-reads changed files only, so files left alone keep what an older engine
                // extracted. Warn and carry on: the git hooks discard this output, so refusing
                // would quietly stop them after every upgrade (upstream 0603ad9e).
                if cg.is_index_stale() {
                    eprintln!("Warning: this index was built by an older CodeGraphV engine, and sync only re-reads changed files. Run \"codegraphV index\" for a full rebuild.");
                }
            }
            cg.close();
        }
        Command::Uninit { path, force } => {
            let root = nodepath::resolve(&[&path]);
            if !directory::is_initialized(&root) {
                println!("No CodeGraphV index in {root}");
                return Ok(());
            }
            if !force && !confirm(&format!("Remove {}?", directory::get_code_graph_dir(&root)))? {
                return Ok(());
            }
            // Owned Git hook blocks go first: they only need the project path, and leaving
            // them behind after removing the index would run `sync` against nothing.
            match codegraph_sync::git_hooks::remove_git_sync_hook(&root, None) {
                Ok(result) if !result.installed.is_empty() => {
                    let names: Vec<_> = result.installed.iter().map(|h| h.as_str()).collect();
                    println!("Removed Git sync hooks: {}", names.join(", "));
                }
                Ok(_) => {}
                Err(error) => eprintln!("Warning: could not remove Git sync hooks: {error}"),
            }
            // A live daemon owns the database; stop it and wait for it to release the endpoint
            // and lock before deleting the directory (A06).
            if codegraph_mcp::daemon::stop(&root).unwrap_or(false) {
                let endpoint = std::path::Path::new(&directory::get_code_graph_dir(&root))
                    .join("native-daemon.json");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while endpoint.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            directory::remove_directory(&root).map_err(|error| {
                format!(
                    "Could not remove the index while another CodeGraphV process is using it ({error}). Stop the MCP server or indexer and retry."
                )
            })?;
            println!("Removed index: {root}");
        }
        Command::Status {
            path,
            json: as_json,
            strict,
        } => {
            // `--strict` (upstream 7889da66): the output is a plain status; only the exit differs.
            let mut strict_failed = false;
            let absolute = nodepath::resolve(&[&path]);
            if !directory::is_initialized(&absolute)
                && directory::find_nearest_code_graph_root(&absolute).is_none()
            {
                if as_json {
                    print_json(
                        &json!({"initialized":false,"version":env!("CARGO_PKG_VERSION"),"projectPath":absolute,"indexPath":directory::get_code_graph_dir(&absolute),"lastIndexed":null}),
                    )?;
                } else {
                    println!(
                        "Not initialized: {absolute}\nRun `codegraphV init` to index this project."
                    );
                }
                // A gate with no index to read has proved nothing.
                strict_failed = true;
            } else {
                let cg = open_read_only(&Project { path: Some(path) }, false)?;
                let status = render::status(&cg)?;
                let coverage = cg.get_language_coverage()?;
                if strict {
                    // A grammar that can't load leaves no row and no zero to count, only files the
                    // index doesn't have yet, so "up to date" is part of the gate. Parse errors are
                    // not: one bad fixture shouldn't fail a whole project.
                    let changes = cg.get_changed_files()?;
                    let pending = changes.added.len() + changes.modified.len() + changes.removed.len();
                    strict_failed = status["rebuildInProgress"] == true
                        || matches!(cg.get_index_state().map(|s| s.as_str()), Some("indexing" | "partial" | "failed"))
                        || pending > 0
                        || !cg.get_index_health()?.needs_reindex.is_empty()
                        || !coverage.unparsed_languages.is_empty();
                }
                if as_json {
                    print_json(&status)?;
                } else {
                    println!("Project: {}\nFiles: {}\nSymbols: {}\nEdges: {}\nBackend: {}\nIndex state: {}",cg.project_root(),status["fileCount"],status["nodeCount"],status["edgeCount"],status["backend"],status["index"]["state"]);
                    print_language_coverage(&cg, &coverage)?;
                    if status["rebuildInProgress"] == true {
                        println!("\n{REBUILD_NOTE}");
                    } else if cg.is_index_stale() {
                        // Sync never advances the extraction stamp, so only a full index helps
                        // (upstream 0410b310).
                        let built = cg.get_index_build_info().version.filter(|v| !v.is_empty())
                            .map_or_else(|| "an earlier version".to_string(), |v| format!("v{}", v.trim_start_matches('v')));
                        println!("\nIndex was built by {built}; re-index to pick up this engine's improvements.\nRun \"codegraphV index\" for a full rebuild");
                    }
                    if let Some(changes) = cg.get_framework_changes_since_index().filter(|_| status["rebuildInProgress"] != true) {
                        let changed = [("added", &changes.added), ("removed", &changes.removed)]
                            .iter()
                            .filter(|(_, names)| !names.is_empty())
                            .map(|(what, names)| format!("{what} {}", names.join(", ")))
                            .collect::<Vec<_>>()
                            .join("; ");
                        println!("\nFrameworks changed since the last full index ({changed}); files indexed before then were read without the change.\nRun `codegraphV index` to re-index every file with them.");
                    }
                }
                cg.close();
            }
            if strict && strict_failed {
                std::process::exit(1);
            }
        }
        Command::Query {
            search,
            project,
            limit,
            kind,
            json: as_json,
        } => {
            let cg = open_read_only(&project, true)?;
            // One effective limit drives the query, the sentinel row, the warning and output.
            // The cap is a hard ceiling; a request above it must not look complete.
            let effective = limit.clamp(1, 10_000);
            let results = cg.search_nodes(
                &search,
                &SearchOptions {
                    limit: Some(effective + 1),
                    kinds: kind
                        .as_deref()
                        .map(|k| {
                            NodeKind::parse(k).ok_or_else(|| format!("Unknown node kind: {k}"))
                        })
                        .transpose()?
                        .map(|k| vec![k]),
                    ..Default::default()
                },
            )?;
            let truncated = results.len() > effective;
            if truncated {
                let requested = if effective != limit {
                    format!(" (requested {limit}; maximum {effective})")
                } else {
                    String::new()
                };
                eprintln!("Results truncated at {effective}{requested}; pass --limit to widen.");
            }
            if as_json {
                print_json(&results.into_iter().take(effective).collect::<Vec<_>>())?;
            } else {
                println!("\nSearch Results for \"{search}\":\n");
                for r in results.into_iter().take(effective) {
                    println!(
                        "{:<12} {}\n  {}:{}",
                        r.node.kind.as_str(),
                        r.node.name,
                        r.node.file_path,
                        r.node.start_line
                    );
                    if let Some(sig) = r.node.signature {
                        println!("  {sig}");
                    }
                    println!();
                }
            }
            cg.close();
        }
        Command::Explore {
            query,
            project,
            max_files,
            column,
            source_hash,
        } => {
            let mut args = json!({"query":query.join(" ")});
            if let Some(n) = max_files {
                args["maxFiles"] = json!(n);
            }
            if let Some(n) = column {
                args["column"] = json!(n);
            }
            if let Some(hash) = source_hash {
                args["sourceHash"] = json!(hash);
            }
            tool(project, "codegraphV_explore", args)?;
        }
        Command::Node {
            name,
            project,
            file,
            offset,
            limit,
            symbols_only,
        } => {
            let mut args = json!({"includeCode":true,"symbolsOnly":symbols_only});
            if let Some(s) = name {
                args["symbol"] = json!(s);
            }
            if let Some(f) = file {
                args["file"] = json!(f);
            }
            if let Some(n) = offset {
                args["offset"] = json!(n);
            }
            if let Some(n) = limit {
                args["limit"] = json!(n);
            }
            tool(project, "codegraphV_node", args)?;
        }
        Command::Files {
            project,
            filter,
            pattern,
            tests,
            no_tests,
            format,
            max_depth,
            no_metadata,
            json: as_json,
        } => {
            // `--tests` / `--no-tests` filter before any output format (upstream b6ea6179).
            let only_tests = (tests || no_tests).then_some(tests);
            let mut args = json!({"path":filter,"pattern":pattern,"format":format,"maxDepth":max_depth,"includeMetadata":!no_metadata});
            if let Some(only) = only_tests {
                args["tests"] = json!(only);
            }
            if as_json {
                let cg = open_read_only(&project, true)?;
                let mut files = wire::files(&cg, filter.as_deref(), pattern.as_deref())?;
                if let Some(only) = only_tests {
                    files.retain(|f| codegraph_search::query_utils::is_test_path(&f.path) == only);
                }
                print_json(&wire::file_json(&files))?;
                cg.close();
            } else {
                tool(project, "codegraphV_files", args)?;
            }
        }
        Command::Callers(s) => symbol_command(s, true)?,
        Command::Callees(s) => symbol_command(s, false)?,
        Command::Implementations {
            symbol,
            project,
            file,
            depth,
            json: as_json,
        } => {
            let cg = open_read_only(&project, true)?;
            let request = implementations::Request { symbol: &symbol, file: file.as_deref(), depth, json: as_json };
            println!("{}", implementations::run(&cg, &request)?);
            cg.close();
        }
        Command::Impact {
            symbol,
            project,
            file,
            depth,
            json: as_json,
        } => {
            let args = json!({"symbol":symbol,"file":file,"depth":depth});
            let cg = open_read_only(&project, true)?;
            if as_json {
                print_json(&wire::impact(&cg, &args)?)?;
            } else {
                println!("{}", render::impact(&cg, &args)?);
            }
            cg.close();
        }
        Command::Context {
            task,
            project,
            format,
            max_nodes,
            no_code,
        } => {
            let cg = open_read_only(&project, true)?;
            let result = cg.build_context(
                &TaskInput::Text(task.join(" ")),
                &BuildContextOptions {
                    format: Some(if format == "json" {
                        ContextFormat::Json
                    } else {
                        ContextFormat::Markdown
                    }),
                    max_nodes,
                    include_code: Some(!no_code),
                    ..Default::default()
                },
            )?;
            match result {
                codegraph_context::BuildOutput::Text(s) => println!("{s}"),
                codegraph_context::BuildOutput::Context(context) => print_json(&context)?,
            };
            cg.close();
        }
        Command::Affected {
            mut files,
            project,
            stdin,
            depth,
            filter,
            json: as_json,
            quiet,
        } => {
            let cg = open_read_only(&project, true)?;
            if stdin {
                let mut input = String::new();
                io::stdin().read_to_string(&mut input)?;
                files.extend(
                    input
                        .lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
            }
            let affected = wire::affected(&cg, &files, depth, filter.as_deref())?;
            let tests = affected["affectedTests"].as_array().unwrap();
            if as_json {
                print_json(&affected)?;
            } else if quiet {
                for file in tests {
                    println!("{}", file.as_str().unwrap());
                }
            } else {
                println!(
                    "{} affected test files:\n{}",
                    tests.len(),
                    tests
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            cg.close();
        }
        Command::Serve {
            project,
            mcp,
            no_watch,
            no_telemetry,
        } => {
            let _ = no_telemetry; // no telemetry is emitted by the native runtime
            if !mcp {
                return Err(
                    "Pass --mcp to serve the MCP protocol over standard input/output.".into(),
                );
            }
            // Exit when the agent that started this server is gone (BUG-89).
            codegraph_core::supervision::start();
            let path = project.path.map(|path| nodepath::resolve(&[&path]));
            let from = path.clone().unwrap_or_else(|| nodepath::resolve(&["."]));
            let shared = directory::resolve_server_root(&from, true).root;
            let proxied = match shared {
                Some(shared) if !no_watch && std::env::var("CODEGRAPHV_DAEMON").as_deref() != Ok("0") => {
                    codegraph_mcp::daemon::proxy(&shared, &std::env::current_exe()?)?
                }
                _ => false,
            };
            if !proxied {
                codegraph_mcp::session::serve(
                    io::BufReader::new(io::stdin()),
                    io::stdout().lock(),
                    path,
                    !no_watch,
                )?;
            }
        }
        Command::DaemonServer { path, idle_timeout } => {
            // A launcher never starts a daemon for a sensitive folder (see `Serve`). One started
            // by hand for such a folder leaves rather than take the folder's locks and serve it.
            if let Some(refusal) = codegraph_core::utils::sensitive_directory_refusal(&path)
                .or_else(|| directory::resolve_server_root(&path, true).refusal)
            {
                eprintln!("[CodeGraphV daemon] {refusal}; exiting.");
                return Ok(());
            }
            let stop = Arc::new(AtomicBool::new(false));
            let signal = stop.clone();
            ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
            codegraph_mcp::daemon::run(&path, &stop, std::time::Duration::from_secs(idle_timeout))?;
        }
        Command::Daemon {
            project,
            stop,
            json: as_json,
        } => {
            let root = root(project.path.as_deref())?;
            if stop {
                println!(
                    "{}",
                    if codegraph_mcp::daemon::stop(&root)? {
                        "Stopped native daemon."
                    } else {
                        "No native daemon is running."
                    }
                );
            } else if let Some(status) = codegraph_mcp::daemon::status(&root) {
                if as_json {
                    print_json(&status)?;
                } else {
                    println!(
                        "Native daemon PID {}\nProject: {}\nClients: {}",
                        status["pid"], status["projectPath"], status["clients"]
                    );
                }
            } else if as_json {
                print_json(&json!({"running":false,"projectPath":root}))?;
            } else {
                println!("No native daemon is running for {root}.");
            }
        }
        Command::Unlock { path } => {
            let root = root(Some(&path))?;
            let lock = std::path::Path::new(&directory::get_code_graph_dir(&root)).join("codegraph.lock");
            if !lock.exists() {
                println!("No database lock present.");
            } else {
                let mut guard = codegraph_core::utils::FileLock::new(&lock);
                guard.acquire()?;
                guard.release();
                println!("Removed stale database lock.");
            }
        }
        Command::Ui {
            path,
            port,
            no_open,
            read_only,
        } => {
            if std::env::var("CODEGRAPHV_UI").as_deref() != Ok("1") {
                return Err(
                    "The viewer is not released yet. Set CODEGRAPHV_UI=1 to use the preview.".into(),
                );
            }
            // Sensitive-directory refusal before anything opens: the guard the MCP entry points
            // use, so `codegraphV ui ~/.ssh` (or a link to it) is turned away here rather than
            // becoming a browsable view of it.
            if let Some(reason) = codegraph_core::utils::validate_project_path(&nodepath::resolve(&[&path])) {
                return Err(reason.into());
            }
            let root = root(Some(&path))?;
            if let Some(reason) = codegraph_core::utils::validate_project_path(&root) {
                return Err(reason.into());
            }
            let viewer = codegraph_ui_server::server::viewer_directory()?;
            let stop = Arc::new(AtomicBool::new(false));
            let signal = stop.clone();
            ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
            let options = codegraph_ui_server::Options {
                root,
                port: port.unwrap_or(4747),
                port_fallback: port.is_none(),
                viewer,
                read_only,
            };
            codegraph_ui_server::serve(options, &stop, |port| {
                let url = format!("http://127.0.0.1:{port}");
                println!("CodeGraphV viewer: {url}\nPress Ctrl-C to stop.");
                if !no_open {
                    open_browser(&url);
                }
            })?;
        }
        Command::Install {
            target,
            location,
            yes,
            init,
            no_permissions,
            prompt_hook,
            print_config,
            refresh,
        } => {
            use codegraph_installer::{Environment, InstallOptions, Location};
            let environment = Environment::current()?;
            let loc = if location.as_deref() == Some("local") {
                Location::Local
            } else {
                Location::Global
            };
            if let Some(id) = print_config {
                println!("{}", environment.print_config(&id, loc)?);
                return Ok(());
            }
            let mut targets = environment.resolve_targets(&target, loc)?;
            if refresh {
                targets.retain(|id| environment.configured(id, loc));
            }
            if !yes
                && !refresh
                && !targets.is_empty()
                && !confirm(&format!(
                    "Configure {} at {} scope?",
                    targets.join(", "),
                    if loc == Location::Global {
                        "global"
                    } else {
                        "local"
                    }
                ))?
            {
                return Ok(());
            }
            // Claude Code's front-load prompt hook: opt-in, default yes; --yes turns it on and a
            // refresh leaves the earlier choice alone.
            let prompt_hook = if let Some(choice) = prompt_hook.as_deref() {
                match choice { "on" => Some(true), "off" => Some(false), _ => None }
            } else if refresh || !targets.iter().any(|t| t == "claude") {
                None
            } else if yes {
                Some(true)
            } else {
                Some(confirm_default_yes(
                    "Front-load CodeGraphV on \"how / where / trace\" prompts? Auto-injects structural context so answers need fewer steps (adds a moment to those prompts; Claude Code only).",
                )?)
            };
            let requests: Vec<_> = targets
                .into_iter()
                .map(|id| {
                    (
                        id,
                        InstallOptions {
                            auto_allow: !no_permissions,
                            preserve_permissions: refresh && !no_permissions,
                            prompt_hook,
                            dry_run: false,
                        },
                    )
                })
                .collect();
            for (id, result) in environment.apply_many(&requests, loc, false)? {
                for file in result.files {
                    println!("{id}: {} {}", file.action, file.path.display());
                }
                for note in result.notes {
                    println!("{note}");
                }
            }
            if init {
                let root = environment.project.to_string_lossy();
                if let Some(reason) = directory::unsafe_index_root_reason(&root) {
                    return Err(reason.into());
                }
                let mut cg = if directory::is_initialized(&root) {
                    CodeGraph::open(&root, OpenOptions::default())?
                } else {
                    CodeGraph::init(&root)?
                };
                let result = cg.index_all(&IndexOptions::default(), None, None)?;
                print_json(&result)?;
                cg.close();
                if !result.success {
                    return Err("Indexing did not complete.".into());
                }
            }
        }
        Command::Uninstall {
            target,
            location,
            yes,
            keep_cli,
        } => {
            use codegraph_installer::{Environment, Location};
            let environment = Environment::current()?;
            let loc = if location.as_deref() == Some("local") {
                Location::Local
            } else {
                Location::Global
            };
            let targets = environment.resolve_targets(&target, loc)?;
            if !yes
                && !confirm(&format!(
                    "Remove CodeGraphV configuration for {}?",
                    targets.join(", ")
                ))?
            {
                return Ok(());
            }
            for (id, result) in environment.remove_many_as(
                &codegraph_installer::CODEGRAPHV,
                &targets,
                loc,
                false,
            )? {
                for file in result.files {
                    println!("{id}: {} {}", file.action, file.path.display());
                }
                for note in result.notes {
                    println!("{note}");
                }
            }
            // A selected integration cannot establish ownership of the shared executable.
            // Only explicit all-target removal may remove it, and never with known consumers.
            let remaining = codegraph_installer::TARGETS.iter().any(|id|
                environment.configured(id, Location::Global)
                    || environment.supports(id, Location::Local) && environment.configured(id, Location::Local));
            if loc == Location::Global && !keep_cli && target == "all" && !remaining {
                let exe = std::env::current_exe()?;
                let env = |name: &str| std::env::var(name).ok();
                match codegraph_types::paths::home_dir() {
                    Some(home) => {
                        let plan = remove_cli::plan(&exe, &env, &home);
                        remove_cli::execute(&plan, &exe, &env, &home);
                    }
                    None => println!("No home directory found; the codegraphV CLI was left installed."),
                }
            } else if loc == Location::Global && !keep_cli {
                println!("CodeGraphV CLI retained: selected integrations or remaining consumers may share it.");
            }
        }
    }
    Ok(())
}
fn symbol_command(s: Symbol, callers: bool) -> Result<()> {
    let cg = open_read_only(&s.project, true)?;
    let args = json!({"symbol":s.symbol,"file":s.file,"limit":s.limit});
    if s.json {
        print_json(&wire::relations(&cg, &args, callers)?)?;
    } else {
        println!("{}", render::relations(&cg, &args, callers)?);
    }
    cg.close();
    Ok(())
}
/// A yes/no question whose default (Enter) is yes; no terminal answers yes, as --yes would.
fn confirm_default_yes(message: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(true);
    }
    print!("{message} [Y/n] ");
    io::stdout().flush()?;
    let mut reply = String::new();
    io::stdin().read_line(&mut reply)?;
    Ok(!matches!(reply.trim().to_lowercase().as_str(), "n" | "no"))
}
fn confirm(message: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Err("This command needs confirmation. Pass --yes (or --force for uninit) for a non-interactive invocation.".into());
    }
    print!("{message} [y/N] ");
    io::stdout().flush()?;
    let mut reply = String::new();
    io::stdin().read_line(&mut reply)?;
    Ok(matches!(reply.trim().to_lowercase().as_str(), "y" | "yes"))
}
fn open_browser(url: &str) {
    let override_browser = std::env::var("CODEGRAPHV_BROWSER").ok();
    if override_browser
        .as_deref()
        .is_some_and(|s| matches!(s, "" | "none" | "0" | "false" | "off"))
    {
        return;
    }
    let mut command = if let Some(browser) = override_browser {
        std::process::Command::new(browser)
    } else if cfg!(windows) {
        std::process::Command::new("explorer.exe")
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    match command.arg(url).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("Could not open browser: {e}. Open {url} manually."),
    }
}
