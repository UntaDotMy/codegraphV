//! Throughput probe for the preParse hooks: `preparse_bench <language> <dir>`.

use codegraph_types::Language;
use std::time::Instant;

fn walk(dir: &std::path::Path, ext: &[&str], out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, ext, out);
        } else if p.extension().and_then(|s| s.to_str()).is_some_and(|x| ext.contains(&x)) {
            out.push(p);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let lang = Language::parse(&args[1]).unwrap();
    let ext: Vec<&str> = match args[1].as_str() {
        "csharp" => vec!["cs"],
        "c" => vec!["c", "h"],
        "cpp" => vec!["cpp", "hpp", "cc", "h"],
        "kotlin" => vec!["kt"],
        _ => vec!["swift"],
    };
    let mut files = Vec::new();
    walk(std::path::Path::new(&args[2]), &ext, &mut files);
    files.sort();
    files.truncate(args.get(3).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX));
    if std::env::var_os("STAGES").is_some() {
        codegraph_extract::enable_preparse_stage_timing();
    }
    let sources: Vec<String> = files.iter().filter_map(|f| std::fs::read_to_string(f).ok()).collect();
    let total: usize = sources.iter().map(|s| s.len()).sum();
    let t = Instant::now();
    let mut changed = 0;
    let (mut ascii_ms, mut other_ms, mut ascii_bytes, mut other_bytes) = (0.0, 0.0, 0usize, 0usize);
    for (f, s) in files.iter().zip(&sources) {
        let t1 = Instant::now();
        let out = codegraph_extract::pre_parse(&f.to_string_lossy(), s, lang);
        let ms = t1.elapsed().as_secs_f64() * 1000.0;
        if s.is_ascii() {
            ascii_ms += ms;
            ascii_bytes += s.len();
        } else {
            other_ms += ms;
            other_bytes += s.len();
        }
        if &out != s {
            changed += 1;
        }
    }
    println!("{} files, {:.1} MB, {} changed, {:.0} ms", sources.len(), total as f64 / 1e6, changed, t.elapsed().as_secs_f64() * 1000.0);
    for (name, ms) in codegraph_extract::preparse_stage_timings().iter().take(14) {
        println!("    {ms:9.1} ms  {name}");
    }
    println!(
        "  ascii files: {:.1} MB in {:.0} ms   non-ascii files: {:.1} MB in {:.0} ms",
        ascii_bytes as f64 / 1e6,
        ascii_ms,
        other_bytes as f64 / 1e6,
        other_ms
    );
}
