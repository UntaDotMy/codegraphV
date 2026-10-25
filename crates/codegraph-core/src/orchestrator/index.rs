//! Full and single-file indexing — `indexAll`, `indexFiles`, `indexFile`, `indexFileWithContent`.
//!
//! The TypeScript fans parsing out to a pool of worker threads and commits the results on the
//! main thread; this port does the same with scoped threads. Results are COMMITTED in original
//! file order, not parse-completion order: resolution disambiguates an ambiguous reference to one
//! of several same-named candidates by the nodes' insertion order, so a stable commit order keeps
//! the resulting graph deterministic (#1015) instead of drifting with parse timing. The buffer of
//! out-of-order results is bounded by a window the workers wait on.

use super::git_state::{is_mpeg_ts_bytes, read_source_or_stamp};
use super::{ExtractionOrchestrator, FileStat, FrameworkGate, IndexPhase, IndexProgress, IndexResult, ProgressFn, UnsupportedExtension, WINDOW_PER_WORKER};
use crate::errors::{CodeGraphError, Result};
use crate::extract::{extract_from_source, has_grammar_load_failure};
use crate::file_limits::{max_source_file_bytes, oversize_stamp, read_bounded_source};
use crate::utils::{validate_path_within_root, ValidateOptions};
use codegraph_extract::languages::{detect_language, is_file_level_only_language, is_language_supported};
use codegraph_extract::scan::config::load_extension_overrides;
use codegraph_extract::scan::{scan_directory, ScanSkipStats};
use codegraph_resolve::frameworks::all_framework_resolvers;
use codegraph_types::jscompat::js_trim_start;
use codegraph_types::log::{log_debug, log_warn};
use codegraph_types::{ExtractionError, ExtractionResult, Language, Severity};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

/// Stack for a parse worker. The walkers recurse per AST level and carry a guard that turns an
/// approaching overflow into a recorded parse error (#1581); a large reserve keeps real-world
/// deeply nested files on the happy side of it. It is address space, not committed memory.
const PARSE_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Files stored per transaction during a full index.
const STORE_GROUP_FILES: usize = 256;

/// `CODEGRAPHV_PARSE_WORKERS`: an explicit worker count (1 = single-threaded); unset or invalid →
/// every core but the one committing results (`cpus - 1`, at least 1).
pub fn resolve_parse_pool_size(env_val: Option<&str>, cpu_count: usize) -> usize {
    if let Some(v) = env_val.filter(|v| !v.is_empty()) {
        if let Ok(n) = v.trim().parse::<f64>() {
            if n.is_finite() && n >= 0.0 {
                return (n.floor() as usize).max(1);
            }
        }
        // non-numeric / negative → fall through to the default
    }
    cpu_count.saturating_sub(1).max(1)
}

/// One file's read-and-parse outcome, produced on a worker and committed in file order.
enum Prepared {
    /// Not a source file after all (an MPEG transport stream named `.ts`, #1910).
    Skipped,
    ReadError(String),
    /// Over the size limit: stored as skipped, its bytes never read (#1910). `content` is the stamp.
    Oversize { content: String, stat: FileStat, language: Language },
    Parsed { content: String, stat: FileStat, language: Language, result: ExtractionResult },
}

struct Shared<'a> {
    root_dir: &'a str,
    files: &'a [String],
    overrides: &'a HashMap<String, Language>,
    gate: &'a FrameworkGate,
    framework_names: &'a [String],
    next: AtomicUsize,
    stop: AtomicBool,
    committed: Mutex<usize>,
    room: Condvar,
    window: usize,
}

fn prepare_file(shared: &Shared<'_>, resolvers: &[codegraph_resolve::frameworks::FrameworkRef], file_path: &str) -> Prepared {
    // Indexing read: follow in-root symlinks the directory walk already descended into (the `../`
    // guard still applies) so files reached via an in-root symlink-to-outside still index (#935).
    let Some(full_path) = validate_path_within_root(shared.root_dir, file_path, ValidateOptions { allow_symlink_escape: true }) else {
        log_warn("Path traversal blocked in batch reader", &[("filePath", file_path.to_string())]);
        return Prepared::ReadError("Path traversal blocked".to_string());
    };
    // Stat first: a file over the size limit is stored as skipped without ever being read or
    // decoded (#1910).
    let read = match read_bounded_source(Path::new(&full_path)) {
        Ok(r) => r,
        Err(e) => return Prepared::ReadError(e.to_string()),
    };
    let stat = FileStat::from_metadata(&read.metadata);
    let Some(bytes) = read.bytes else {
        let content = oversize_stamp(read.size);
        let language = detect_language(file_path, Some(&content), Some(shared.overrides), Some(Path::new(shared.root_dir)));
        return Prepared::Oversize { content, stat: FileStat { size: read.size, ..stat }, language };
    };
    // Read bytes, not text: a `.ts` that is really an MPEG transport stream is recognised from its
    // head here, at no extra I/O, and never decoded or parsed.
    if is_mpeg_ts_bytes(file_path, &bytes) {
        return Prepared::Skipped;
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    drop(bytes);
    let language = detect_language(file_path, Some(&content), Some(shared.overrides), Some(Path::new(shared.root_dir)));
    let names = shared.gate.for_file(file_path, shared.framework_names);
    let result = extract_from_source(file_path, &content, language, &names, resolvers);
    Prepared::Parsed { content, stat, language, result }
}

fn worker_loop(shared: &Shared<'_>, tx: mpsc::Sender<(usize, Prepared)>) {
    // The registry holds `Rc`s, so each worker builds its own.
    let resolvers = all_framework_resolvers();
    loop {
        let i = shared.next.fetch_add(1, Ordering::SeqCst);
        if i >= shared.files.len() || shared.stop.load(Ordering::SeqCst) {
            return;
        }
        // Backpressure on the dispatched-but-not-yet-committed count: buffered items hold whole
        // file contents, so this bound is load-bearing for memory. The file at the commit cursor
        // is never made to wait (its distance is 0), so the window can't deadlock.
        {
            let mut committed = shared.committed.lock().unwrap_or_else(|e| e.into_inner());
            while i >= *committed + shared.window && !shared.stop.load(Ordering::SeqCst) {
                committed = shared.room.wait(committed).unwrap_or_else(|e| e.into_inner());
            }
        }
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let item = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepare_file(shared, &resolvers, &shared.files[i])))
            .unwrap_or_else(|payload| Prepared::ReadError(format!("Worker exited unexpectedly: {}", crate::extract::panic_message(payload.as_ref()))));
        if tx.send((i, item)).is_err() {
            return;
        }
    }
}

/// Running totals of an indexing pass.
#[derive(Default)]
struct Tally {
    processed: usize,
    files_indexed: usize,
    files_skipped: usize,
    files_errored: usize,
    total_nodes: usize,
    total_edges: usize,
    errors: Vec<ExtractionError>,
}

fn size_exceeded(file_path: &str, size: u64) -> ExtractionResult {
    ExtractionResult {
        errors: vec![ExtractionError {
            message: format!("File exceeds max size ({size} > {})", max_source_file_bytes()),
            file_path: Some(file_path.to_string()),
            line: None,
            column: None,
            severity: Severity::Warning,
            code: Some("size_exceeded".to_string()),
        }],
        ..ExtractionResult::default()
    }
}

fn path_traversal(file_path: &str, message: String) -> ExtractionResult {
    ExtractionResult {
        errors: vec![ExtractionError { message, file_path: Some(file_path.to_string()), line: None, column: None, severity: Severity::Error, code: Some("path_traversal".to_string()) }],
        ..ExtractionResult::default()
    }
}

fn is_retryable(e: &ExtractionError) -> bool {
    e.code.as_deref() == Some("parse_error")
        && e.file_path.is_some()
        && (e.message.contains("Worker exited") || e.message.contains("memory access out of bounds") || e.message.contains("timed out") || e.message.contains("defer:"))
}

impl ExtractionOrchestrator {
    /// Index all files in the project.
    pub fn index_all(&self, mut on_progress: Option<ProgressFn<'_>>, signal: Option<&AtomicBool>, verbose: bool) -> Result<IndexResult> {
        let start = Instant::now();
        let root = Path::new(&self.root_dir);
        let aborted_flag = || signal.is_some_and(|s| s.load(Ordering::SeqCst));
        let log = |msg: &str| {
            if verbose {
                println!("[worker] {msg}");
            }
        };
        let mut emit = |p: IndexProgress| {
            if let Some(cb) = on_progress.as_deref_mut() {
                cb(&p);
            }
        };

        // Custom extension → language overrides from the project's codegraph.json. Threaded into
        // language detection so custom-extension files load the right grammar and store under the
        // mapped language.
        let overrides = load_extension_overrides(root);

        // Phase 1: Scan for files
        emit(IndexProgress::new(IndexPhase::Scanning, 0, 0));
        let mut skip_stats = ScanSkipStats::default();
        let files = {
            let mut scan_cb = |current: usize, file: &str| emit(IndexProgress::with_file(IndexPhase::Scanning, current, 0, file));
            scan_directory(root, Some(&mut scan_cb), Some(&mut skip_stats)).map_err(CodeGraphError::other)?
        };
        // Only meaningful when nothing was indexable — see IndexResult (#1502).
        let skip_summary = |result: &mut IndexResult| {
            let total: usize = skip_stats.unsupported_by_extension.values().sum();
            if total == 0 {
                return;
            }
            let mut top: Vec<UnsupportedExtension> = skip_stats.unsupported_by_extension.iter().map(|(ext, count)| UnsupportedExtension { ext: ext.clone(), count: *count }).collect();
            top.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.ext.cmp(&b.ext)));
            top.truncate(5);
            result.files_skipped_unsupported = Some(total);
            result.top_unsupported_extensions = Some(top);
        };

        // A re-index over an existing DB skips unchanged-hash files at the store, which would
        // preserve wiped zero-node rows (#1541) — drop them first so this run stores their files
        // fresh. No-op on a fresh DB.
        self.heal_zero_node_rows()?;

        // Detect frameworks once per index_all run using the scanned file list. Names are passed to
        // each parse call so framework-specific extractors (route nodes, middleware, etc.) run
        // after the language pass. Detection is reset each run so adding e.g. requirements.txt
        // between runs is picked up without restarting the process.
        self.reset_detected_frameworks();
        let framework_names = self.ensure_detected_frameworks(Some(files.clone()));

        if aborted_flag() {
            return Ok(IndexResult {
                success: false,
                errors: vec![ExtractionError { message: "Aborted".to_string(), file_path: None, line: None, column: None, severity: Severity::Error, code: None }],
                duration_ms: start.elapsed().as_millis() as u64,
                ..IndexResult::default()
            });
        }

        // Phase 2: Parse files on a pool of worker threads (keeps the main thread free and uses
        // every core).
        let total = files.len();
        emit(IndexProgress::new(IndexPhase::Parsing, 0, total));
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).max(3);
        let workers = resolve_parse_pool_size(std::env::var("CODEGRAPHV_PARSE_WORKERS").ok().as_deref(), cpus).min(total.max(1));
        log(&format!("Parse worker pool: {workers} worker(s)"));

        let gate = self.framework_gate();
        let shared = Shared {
            root_dir: &self.root_dir,
            files: &files,
            overrides: &overrides,
            gate: &gate,
            framework_names: &framework_names,
            next: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            committed: Mutex::new(0),
            room: Condvar::new(),
            window: (workers * WINDOW_PER_WORKER).max(4),
        };

        let mut tally = Tally::default();
        let mut aborted = false;
        let mut failure: Option<CodeGraphError> = None;

        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel::<(usize, Prepared)>();
            for n in 0..workers {
                let tx = tx.clone();
                let shared = &shared;
                let spawned = std::thread::Builder::new().name(format!("codegraph-parse-{n}")).stack_size(PARSE_STACK_BYTES).spawn_scoped(scope, move || worker_loop(shared, tx));
                if spawned.is_err() && n == 0 {
                    // Without even one worker there is nothing to wait for.
                    shared.stop.store(true, Ordering::SeqCst);
                }
            }
            drop(tx);

            let mut pending: BTreeMap<usize, Prepared> = BTreeMap::new();
            let mut next_to_store = 0usize;
            let (mut wait, mut commit) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
            // Files are stored in groups, one transaction each (every file still atomic): a commit
            // per file rewrote the same table and index pages into the log every time. A group
            // is committed before the writer waits, so nothing sits uncommitted while it idles.
            let mut in_group = 0usize;
            'receive: while next_to_store < total {
                if in_group > 0 && !pending.contains_key(&next_to_store) {
                    if let Err(e) = self.store.commit_batch() {
                        failure = Some(e.into());
                        break 'receive;
                    }
                    in_group = 0;
                }
                let waited = Instant::now();
                let Ok((i, item)) = rx.recv() else { break };
                wait += waited.elapsed();
                pending.insert(i, item);
                while let Some(item) = pending.remove(&next_to_store) {
                    let committing = Instant::now();
                    if aborted_flag() {
                        aborted = true;
                        break 'receive;
                    }
                    if in_group == 0 {
                        if let Err(e) = self.store.begin_batch() {
                            failure = Some(e.into());
                            break 'receive;
                        }
                    }
                    let file_path = &files[next_to_store];
                    if let Err(e) = self.commit_prepared(&mut tally, file_path, item, total, &mut emit) {
                        self.store.rollback_batch();
                        in_group = 0;
                        failure = Some(e);
                        break 'receive;
                    }
                    in_group += 1;
                    if in_group >= STORE_GROUP_FILES {
                        if let Err(e) = self.store.commit_batch() {
                            failure = Some(e.into());
                            break 'receive;
                        }
                        in_group = 0;
                    }
                    next_to_store += 1;
                    *shared.committed.lock().unwrap_or_else(|e| e.into_inner()) = next_to_store;
                    shared.room.notify_all();
                    commit += committing.elapsed();
                }
            }
            if in_group > 0 {
                if let Err(e) = self.store.commit_batch() {
                    failure = failure.take().or(Some(e.into()));
                }
            }
            if std::env::var("CODEGRAPHV_SYNTH_TIMINGS").is_ok_and(|v| !v.is_empty()) {
                eprintln!("[phase-timing] parse: {workers} workers, writer waited {}ms, committed {}ms", wait.as_millis(), commit.as_millis());
            }
            // Release any worker still waiting for room, and let the ones mid-parse finish.
            shared.stop.store(true, Ordering::SeqCst);
            shared.room.notify_all();
            drop(rx);
        });

        if let Some(e) = failure {
            return Err(e);
        }
        if aborted || aborted_flag() {
            let mut errors = vec![ExtractionError { message: "Aborted".to_string(), file_path: None, line: None, column: None, severity: Severity::Error, code: None }];
            errors.extend(tally.errors);
            let mut result = IndexResult {
                success: false,
                files_indexed: tally.files_indexed,
                files_skipped: tally.files_skipped,
                files_errored: tally.files_errored,
                files_discovered: Some(total),
                nodes_created: tally.total_nodes,
                edges_created: tally.total_edges,
                errors,
                duration_ms: start.elapsed().as_millis() as u64,
                ..IndexResult::default()
            };
            skip_summary(&mut result);
            return Ok(result);
        }
        if tally.processed < total {
            // A worker that died (a panic outside the extractor's own guard) would otherwise leave
            // the index silently partial; the counts must add up to what the scan found.
            tally.errors.push(ExtractionError {
                message: format!("Parse workers stopped early: {} of {} files were processed", tally.processed, total),
                file_path: None,
                line: None,
                column: None,
                severity: Severity::Error,
                code: Some("worker_died".to_string()),
            });
        }

        // Report 100% so the progress bar doesn't hang at 99%
        emit(IndexProgress::new(IndexPhase::Parsing, total, total));

        // Retry pass: files that failed because the walker ran out of stack (or crashed) may
        // succeed with a comment-stripped copy, which spares the parser most of the pressure on
        // files that are mostly comments (compiler test suites with CHECK directives).
        let retryable: Vec<ExtractionError> = tally.errors.iter().filter(|e| is_retryable(e)).cloned().collect();
        if !retryable.is_empty() {
            log(&format!("Retrying {} files that failed...", retryable.len()));
            let mut still_failing: Vec<ExtractionError> = Vec::new();
            for err_entry in &retryable {
                let file_path = err_entry.file_path.clone().unwrap_or_default();
                if aborted_flag() {
                    break;
                }
                let Some(content) = self.reread(&file_path) else { continue };
                let language = detect_language(&file_path, Some(&content), Some(&overrides), Some(root));
                let names = gate.for_file(&file_path, &framework_names);
                let result = parse_on_big_stack(&file_path, &content, language, &names);
                if !result.nodes.is_empty() || result.errors.is_empty() {
                    let stats = FileStat::of(&root.join(&file_path))?;
                    let (n, e) = (result.nodes.len(), result.edges.len());
                    self.store_extraction_result(&file_path, &content, language, stats, result)?;
                    remove_error(&mut tally.errors, err_entry);
                    tally.files_errored = tally.files_errored.saturating_sub(1);
                    tally.files_indexed += 1;
                    tally.total_nodes += n;
                    tally.total_edges += e;
                    log(&format!("Retry OK: {file_path} ({n} nodes)"));
                } else {
                    still_failing.push(err_entry.clone());
                }
            }

            // Last resort: for files that still fail, strip comment-only lines to reduce the
            // walk's depth. Many compiler test files are 90%+ comments that don't contribute code
            // nodes. Line numbers are preserved by replacing the lines with empty ones so node
            // positions stay correct.
            if !still_failing.is_empty() {
                log(&format!("{} files still failing — retrying with comments stripped...", still_failing.len()));
                for err_entry in &still_failing {
                    let file_path = err_entry.file_path.clone().unwrap_or_default();
                    if aborted_flag() {
                        break;
                    }
                    let Some(full_content) = self.reread(&file_path) else { continue };
                    let stripped = full_content.split('\n').map(|line| if js_trim_start(line).starts_with("//") { "" } else { line }).collect::<Vec<_>>().join("\n");
                    let language = detect_language(&file_path, Some(&full_content), Some(&overrides), Some(root));
                    let names = gate.for_file(&file_path, &framework_names);
                    let result = parse_on_big_stack(&file_path, &stripped, language, &names);
                    if !result.nodes.is_empty() || result.errors.is_empty() {
                        let stats = FileStat::of(&root.join(&file_path))?;
                        let (n, e) = (result.nodes.len(), result.edges.len());
                        self.store_extraction_result(&file_path, &full_content, language, stats, result)?;
                        // Salvaged from comment-stripped source: keep a visible trace in the
                        // summary instead of erasing the failure outright — the stored result may
                        // be missing whatever the failing parse choked on, and a silently "clean"
                        // file here is how an index quietly disagrees with a later per-file sync
                        // of the same bytes (#1565).
                        if let Some(entry) = tally.errors.iter_mut().find(|x| x.file_path == err_entry.file_path && x.message == err_entry.message) {
                            entry.severity = Severity::Warning;
                            entry.code = Some("salvaged_stripped".to_string());
                            entry.message = format!("Indexed from comment-stripped source after repeated parse failures (symbols may be incomplete until the file is re-indexed): {}", err_entry.message);
                        }
                        tally.files_errored = tally.files_errored.saturating_sub(1);
                        tally.files_indexed += 1;
                        tally.total_nodes += n;
                        tally.total_edges += e;
                        log(&format!("Retry (stripped) OK: {file_path} ({n} nodes)"));
                    }
                }
            }
        }

        let mut result = IndexResult {
            success: tally.files_indexed > 0 || !tally.errors.iter().any(|e| e.severity == Severity::Error),
            files_indexed: tally.files_indexed,
            files_skipped: tally.files_skipped,
            files_errored: tally.files_errored,
            files_discovered: Some(total),
            nodes_created: tally.total_nodes,
            edges_created: tally.total_edges,
            errors: tally.errors,
            duration_ms: start.elapsed().as_millis() as u64,
            ..IndexResult::default()
        };
        skip_summary(&mut result);
        Ok(result)
    }

    /// The bounded re-read the retry passes use: `None` when the path is refused, unreadable, or
    /// the file has grown past the limit since the first read (#1910).
    fn reread(&self, file_path: &str) -> Option<String> {
        let full = validate_path_within_root(&self.root_dir, file_path, ValidateOptions::default())?;
        let read = read_bounded_source(Path::new(&full)).ok()?;
        Some(String::from_utf8_lossy(&read.bytes?).into_owned())
    }

    /// Store one prepared file and count it — `storeResult` in the TypeScript.
    fn commit_prepared(&self, tally: &mut Tally, file_path: &str, item: Prepared, total: usize, emit: &mut dyn FnMut(IndexProgress)) -> Result<()> {
        match item {
            Prepared::Skipped => {
                // Not a source file after all — counted as done, stored as nothing.
                tally.processed += 1;
                emit(IndexProgress::new(IndexPhase::Parsing, tally.processed, total));
            }
            Prepared::ReadError(error) => {
                tally.processed += 1;
                tally.files_errored += 1;
                tally.errors.push(ExtractionError {
                    message: format!("Failed to read file: {error}"),
                    file_path: Some(file_path.to_string()),
                    line: None,
                    column: None,
                    severity: Severity::Error,
                    code: Some("read_error".to_string()),
                });
                emit(IndexProgress::new(IndexPhase::Parsing, tally.processed, total));
            }
            Prepared::Oversize { content, stat, language } => {
                // Honour the source-size limit (`max_source_file_bytes`). Without this, vendored generated headers,
                // minified bundles, and other multi-MB files get indexed, wasting the parser on
                // inputs with no useful symbols.
                let result = size_exceeded(file_path, stat.size);
                self.store_result(tally, file_path, &content, language, stat, result, total, emit)?;
            }
            Prepared::Parsed { content, stat, language, result } => {
                self.store_result(tally, file_path, &content, language, stat, result, total, emit)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn store_result(&self, tally: &mut Tally, file_path: &str, content: &str, language: Language, stat: FileStat, mut result: ExtractionResult, total: usize, emit: &mut dyn FnMut(IndexProgress)) -> Result<()> {
        tally.processed += 1;
        let node_count = result.nodes.len();
        let edge_count = result.edges.len();
        // A file whose grammar failed to load was never parsed (#2335): store nothing — a row from
        // an earlier run keeps its data, and with no row (or an older hash) the next sync or index
        // retries the file.
        let grammar_unavailable = has_grammar_load_failure(&result.errors);
        for err in result.errors.iter_mut() {
            if err.file_path.is_none() {
                err.file_path = Some(file_path.to_string());
            }
        }
        let had_error = result.errors.iter().any(|e| e.severity == Severity::Error);
        let result_errors = result.errors.clone();
        if !grammar_unavailable {
            self.store_extraction_result(file_path, content, language, stat, result)?;
        }
        tally.errors.extend(result_errors);

        if grammar_unavailable {
            tally.files_errored += 1;
        } else if node_count > 0 {
            tally.files_indexed += 1;
            tally.total_nodes += node_count;
            tally.total_edges += edge_count;
        } else if had_error {
            tally.files_errored += 1;
        } else if is_file_level_only_language(language) {
            // Files with no symbols but no errors (yaml, twig, properties) are tracked at the file
            // level — count them as indexed so the CLI doesn't misleadingly report "No files
            // found to index".
            tally.files_indexed += 1;
        } else {
            tally.files_skipped += 1;
        }
        emit(IndexProgress::with_file(IndexPhase::Parsing, tally.processed, total, file_path));
        Ok(())
    }

    /// Index specific files.
    pub fn index_files(&self, file_paths: &[String]) -> Result<IndexResult> {
        let start = Instant::now();
        let mut out = IndexResult::default();
        for file_path in file_paths {
            let result = self.index_file(file_path)?;
            out.errors.extend(result.errors.iter().cloned());
            if has_grammar_load_failure(&result.errors) {
                out.files_errored += 1; // nothing was stored (#2335)
            } else if !result.nodes.is_empty() {
                out.files_indexed += 1;
                out.nodes_created += result.nodes.len();
                out.edges_created += result.edges.len();
            } else if result.errors.iter().any(|e| e.severity == Severity::Error) {
                out.files_errored += 1;
            } else {
                match self.store.get_file_by_path(file_path)? {
                    Some(tracked) if is_file_level_only_language(tracked.language) => out.files_indexed += 1,
                    _ => out.files_skipped += 1,
                }
            }
        }
        out.success = out.files_indexed > 0 || !out.errors.iter().any(|e| e.severity == Severity::Error);
        out.duration_ms = start.elapsed().as_millis() as u64;
        Ok(out)
    }

    /// Index a single file.
    pub fn index_file(&self, relative_path: &str) -> Result<ExtractionResult> {
        // Indexing read: follow in-root symlinks (the `../` guard still applies), #935.
        let Some(full_path) = validate_path_within_root(&self.root_dir, relative_path, ValidateOptions { allow_symlink_escape: true }) else {
            return Ok(path_traversal(relative_path, format!("Path traversal blocked: {relative_path}")));
        };
        // Read file content and stats. An oversize file is stored as skipped; its bytes are never
        // needed (#1910).
        let read = match read_bounded_source(Path::new(&full_path)) {
            Ok(r) => r,
            Err(error) => {
                return Ok(ExtractionResult {
                    errors: vec![ExtractionError {
                        message: format!("Failed to read file: {error}"),
                        file_path: Some(relative_path.to_string()),
                        line: None,
                        column: None,
                        severity: Severity::Error,
                        code: Some("read_error".to_string()),
                    }],
                    ..ExtractionResult::default()
                });
            }
        };
        let stat = FileStat { size: read.size, ..FileStat::from_metadata(&read.metadata) };
        let content = match &read.bytes {
            // An MPEG transport stream named `.ts` is not TypeScript (#1910).
            Some(bytes) if is_mpeg_ts_bytes(relative_path, bytes) => return Ok(ExtractionResult::default()),
            Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            None => oversize_stamp(stat.size),
        };
        self.index_file_with_content(relative_path, &content, stat)
    }

    /// Index a single file with pre-read content and stats.
    pub fn index_file_with_content(&self, relative_path: &str, content: &str, stats: FileStat) -> Result<ExtractionResult> {
        // Prevent `../` traversal; follow in-root symlinks like the directory walk (#935).
        if validate_path_within_root(&self.root_dir, relative_path, ValidateOptions { allow_symlink_escape: true }).is_none() {
            log_warn("Path traversal blocked in indexFileWithContent", &[("relativePath", relative_path.to_string())]);
            return Ok(path_traversal(relative_path, "Path traversal blocked".to_string()));
        }
        let root = Path::new(&self.root_dir);
        let language = detect_language(relative_path, Some(content), Some(&load_extension_overrides(root)), Some(root));

        // Check file size
        if stats.size > max_source_file_bytes() {
            let result = size_exceeded(relative_path, stats.size);
            self.store_extraction_result(relative_path, content, language, stats, result.clone())?;
            return Ok(result);
        }

        // Detect language (honoring the project's codegraph.json extension overrides)
        if !is_language_supported(language) {
            return Ok(ExtractionResult::default());
        }

        // Extract from source. Use cached framework names if index_all has run, otherwise detect
        // on the spot so single-file re-index paths still emit route nodes / middleware / etc.
        let names = self.ensure_detected_frameworks(None);
        let names = self.framework_gate().for_file(relative_path, &names);
        let result = parse_on_big_stack(relative_path, content, language, &names);

        // Store in database
        self.store_extraction_result(relative_path, content, language, stats, result.clone())?;
        Ok(result)
    }

    /// The file's content as change detection sees it (see [`read_source_or_stamp`]).
    #[allow(dead_code)]
    pub(crate) fn read_for_hash(&self, relative_path: &str) -> std::io::Result<Option<String>> {
        read_source_or_stamp(&Path::new(&self.root_dir).join(relative_path), relative_path)
    }
}

fn remove_error(errors: &mut Vec<ExtractionError>, target: &ExtractionError) {
    if let Some(i) = errors.iter().position(|e| e.file_path == target.file_path && e.message == target.message) {
        errors.remove(i);
    }
}

/// Parse on a thread with a worker-sized stack: the single-file and retry paths run on the
/// caller's thread otherwise, which may be a small one. `FrameworkRef` is an `Rc`, so the
/// registry is built inside the thread.
fn parse_on_big_stack(file_path: &str, content: &str, language: Language, names: &[String]) -> ExtractionResult {
    let outcome = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(PARSE_STACK_BYTES)
            .spawn_scoped(scope, || {
                let resolvers = all_framework_resolvers();
                extract_from_source(file_path, content, language, names, &resolvers)
            })
            .map(|h| h.join())
    });
    match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(_)) | Err(_) => {
            log_debug("Parse thread failed", &[("filePath", file_path.to_string())]);
            ExtractionResult {
                errors: vec![ExtractionError {
                    message: "Worker exited unexpectedly".to_string(),
                    file_path: Some(file_path.to_string()),
                    line: None,
                    column: None,
                    severity: Severity::Error,
                    code: Some("parse_error".to_string()),
                }],
                ..ExtractionResult::default()
            }
        }
    }
}

#[cfg(test)]
mod pool_size_tests {
    use super::resolve_parse_pool_size;

    #[test]
    fn the_default_uses_every_core_but_one_and_the_variable_overrides() {
        assert_eq!(resolve_parse_pool_size(None, 12), 11);
        assert_eq!(resolve_parse_pool_size(None, 64), 63);
        assert_eq!(resolve_parse_pool_size(None, 1), 1);
        assert_eq!(resolve_parse_pool_size(Some("32"), 12), 32);
        assert_eq!(resolve_parse_pool_size(Some("0"), 12), 1);
        assert_eq!(resolve_parse_pool_size(Some("lots"), 12), 11);
    }
}
