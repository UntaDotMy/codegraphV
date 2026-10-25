use clap::{Args, Parser, Subcommand};
mod prompt_hook;
mod upgrade;
mod wire;
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
    name = "codegraph",
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
        #[arg(short, long)]
        yes: bool,
        #[arg(short, long, hide = true)]
        index: bool,
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
    /// Remove this project's index and generated integration files.
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
        #[arg(long,default_value="tree",value_parser=["tree","flat","grouped"])]
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
    #[command(alias="web",hide=std::env::var("CODEGRAPH_UI").as_deref()!=Ok("1"))]
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
    /// Connect CodeGraph to coding agents.
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
        #[arg(long)]
        print_config: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
    /// Remove CodeGraph configuration from coding agents.
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
    /// Claude UserPromptSubmit hook: inject CodeGraph context for structural prompts (reads
    /// {prompt,cwd} JSON on stdin).
    #[command(hide = true)]
    PromptHook,
    /// Update codegraph to the latest release, or to the version given.
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
        format!("Project is not indexed: {absolute}. Run `codegraph init` first.").into()
    })
}
fn open(project: &Project) -> Result<CodeGraph> {
    Ok(CodeGraph::open(
        &root(project.path.as_deref())?,
        OpenOptions::default(),
    )?)
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn tool(project: Project, name: &str, args: Value) -> Result<()> {
    let path = root(project.path.as_deref())?;
    let result = ToolHandler::new(Some(path), false).call(name, &args);
    if result.is_error {
        return Err(result.message().into());
    }
    println!("{}", result.message());
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
            ..
        }
        | Command::Index {
            path,
            force,
            verbose,
            ..
        } => {
            let root = nodepath::resolve(&[&path]);
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
            print_json(&result)?;
            cg.close();
            if !result.success {
                return Err("Indexing did not complete; see the errors above.".into());
            }
        }
        Command::Sync { path, quiet } => {
            let mut cg = open(&Project { path: Some(path) })?;
            let result = cg.sync(&IndexOptions::default(), None)?;
            if !quiet {
                print_json(&result)?;
            }
            cg.close();
        }
        Command::Uninit { path, force } => {
            let root = nodepath::resolve(&[&path]);
            if !directory::is_initialized(&root) {
                println!("No CodeGraph index in {root}");
                return Ok(());
            }
            if !force && !confirm(&format!("Remove {}?", directory::get_code_graph_dir(&root)))? {
                return Ok(());
            }
            directory::remove_directory(&root)?;
            println!("Removed index: {root}");
        }
        Command::Status {
            path,
            json: as_json,
        } => {
            let absolute = nodepath::resolve(&[&path]);
            if !directory::is_initialized(&absolute)
                && directory::find_nearest_code_graph_root(&absolute).is_none()
            {
                if as_json {
                    print_json(
                        &json!({"initialized":false,"version":env!("CARGO_PKG_VERSION"),"projectPath":absolute,"indexPath":std::path::Path::new(&absolute).join(".codegraph"),"lastIndexed":null}),
                    )?;
                } else {
                    println!(
                        "Not initialized: {absolute}\nRun `codegraph init` to index this project."
                    );
                }
            } else {
                let cg = open(&Project { path: Some(path) })?;
                let status = render::status(&cg)?;
                if as_json {
                    print_json(&status)?;
                } else {
                    println!("Project: {}\nFiles: {}\nSymbols: {}\nEdges: {}\nBackend: {}\nIndex state: {}",cg.project_root(),status["fileCount"],status["nodeCount"],status["edgeCount"],status["backend"],status["index"]["state"]);
                }
                cg.close();
            }
        }
        Command::Query {
            search,
            project,
            limit,
            kind,
            json: as_json,
        } => {
            let cg = open(&project)?;
            let results = cg.search_nodes(
                &search,
                &SearchOptions {
                    limit: Some(limit.clamp(1, 10000) + 1),
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
            if results.len() > limit {
                eprintln!("Results truncated at {limit}; pass --limit to widen.");
            }
            if as_json {
                print_json(&results.into_iter().take(limit).collect::<Vec<_>>())?;
            } else {
                println!("\nSearch Results for \"{search}\":\n");
                for r in results.into_iter().take(limit) {
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
        } => {
            let mut args = json!({"query":query.join(" ")});
            if let Some(n) = max_files {
                args["maxFiles"] = json!(n);
            }
            tool(project, "codegraph_explore", args)?;
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
            tool(project, "codegraph_node", args)?;
        }
        Command::Files {
            project,
            filter,
            pattern,
            format,
            max_depth,
            no_metadata,
            json: as_json,
        } => {
            let args = json!({"path":filter,"pattern":pattern,"format":format,"maxDepth":max_depth,"includeMetadata":!no_metadata});
            if as_json {
                let cg = open(&project)?;
                let files = wire::files(&cg, filter.as_deref(), pattern.as_deref())?;
                print_json(&wire::file_json(&files))?;
                cg.close();
            } else {
                tool(project, "codegraph_files", args)?;
            }
        }
        Command::Callers(s) => symbol_command(s, true)?,
        Command::Callees(s) => symbol_command(s, false)?,
        Command::Impact {
            symbol,
            project,
            file,
            depth,
            json: as_json,
        } => {
            let args = json!({"symbol":symbol,"file":file,"depth":depth});
            let cg = open(&project)?;
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
            let cg = open(&project)?;
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
            let cg = open(&project)?;
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
        } => {
            if !mcp {
                return Err(
                    "Pass --mcp to serve the MCP protocol over standard input/output.".into(),
                );
            }
            let path = project.path.map(|path| nodepath::resolve(&[&path]));
            let from = path.clone().unwrap_or_else(|| nodepath::resolve(&["."]));
            let shared = directory::resolve_server_root(&from, true).root;
            if !no_watch
                && std::env::var("CODEGRAPH_DAEMON").as_deref() != Ok("0")
                && shared.is_some()
            {
                codegraph_mcp::daemon::proxy(&shared.unwrap(), &std::env::current_exe()?)?;
            } else {
                codegraph_mcp::session::serve(
                    io::BufReader::new(io::stdin()),
                    io::stdout().lock(),
                    path,
                    !no_watch,
                )?;
            }
        }
        Command::DaemonServer { path, idle_timeout } => {
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
            let lock = std::path::Path::new(&root).join(".codegraph/codegraph.lock");
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
            if std::env::var("CODEGRAPH_UI").as_deref() != Ok("1") {
                return Err(
                    "The viewer is not released yet. Set CODEGRAPH_UI=1 to use the preview.".into(),
                );
            }
            let root = root(Some(&path))?;
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
                println!("CodeGraph viewer: {url}\nPress Ctrl-C to stop.");
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
            let prompt_hook = if refresh || !targets.iter().any(|t| t == "claude") {
                None
            } else if yes {
                Some(true)
            } else {
                Some(confirm_default_yes(
                    "Front-load CodeGraph on \"how / where / trace\" prompts? Auto-injects structural context so answers need fewer steps (adds a moment to those prompts; Claude Code only).",
                )?)
            };
            for id in targets {
                let result = environment.apply(
                    &id,
                    loc,
                    InstallOptions {
                        auto_allow: !no_permissions,
                        prompt_hook,
                    },
                    false,
                )?;
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
            ..
        } => {
            use codegraph_installer::{Environment, InstallOptions, Location};
            let environment = Environment::current()?;
            let loc = if location.as_deref() == Some("local") {
                Location::Local
            } else {
                Location::Global
            };
            let targets = environment.resolve_targets(&target, loc)?;
            if !yes
                && !confirm(&format!(
                    "Remove CodeGraph configuration for {}?",
                    targets.join(", ")
                ))?
            {
                return Ok(());
            }
            for id in targets {
                let result = environment.apply(&id, loc, InstallOptions::default(), true)?;
                for file in result.files {
                    println!("{id}: {} {}", file.action, file.path.display());
                }
                for note in result.notes {
                    println!("{note}");
                }
            }
        }
    }
    Ok(())
}
fn symbol_command(s: Symbol, callers: bool) -> Result<()> {
    let cg = open(&s.project)?;
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
    let override_browser = std::env::var("CODEGRAPH_BROWSER").ok();
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
