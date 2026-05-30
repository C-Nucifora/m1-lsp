//! m1-lint — command-line linter for the MoTeC M1 script language.

use std::path::PathBuf;
use std::process;

use m1_lint::config::Config;
use m1_lint::registry::Registry;
use m1_lint::report;
use m1_lint::runner::Runner;

enum Format {
    Human,
    Json,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("m1-lint {}", env!("CARGO_PKG_VERSION"));
        process::exit(0);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        process::exit(0);
    }

    let mut format = Format::Human;
    let mut do_fix = false;
    let mut config_path: Option<PathBuf> = None;
    let mut max_line: Option<usize> = None;
    let mut max_depth: Option<usize> = None;
    let mut max_complexity: Option<u32> = None;
    let mut select: Option<Vec<String>> = None;
    let mut ignore: Option<Vec<String>> = None;
    let mut files: Vec<PathBuf> = Vec::new();

    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fix" => do_fix = true,
            "--format" => match it.next().map(String::as_str) {
                Some("human") => format = Format::Human,
                Some("json") => format = Format::Json,
                other => fail(&format!("--format expects human|json, got {other:?}")),
            },
            "--config" => config_path = Some(PathBuf::from(req(it.next(), "--config"))),
            "--max-line-length" => max_line = Some(parse_num(it.next(), "--max-line-length")),
            "--max-nesting-depth" => max_depth = Some(parse_num(it.next(), "--max-nesting-depth")),
            "--max-complexity" => max_complexity = Some(parse_num(it.next(), "--max-complexity")),
            "--select" => select = Some(split_codes(it.next(), "--select")),
            "--ignore" => ignore = Some(split_codes(it.next(), "--ignore")),
            s if s.starts_with("--") => fail(&format!("unknown flag: {s}")),
            s => files.push(PathBuf::from(s)),
        }
    }
    if files.is_empty() {
        fail("no input files");
    }

    let mut any_error = false;
    let mut json_files: Vec<(String, m1_lint::runner::RunResult)> = Vec::new();

    for path in &files {
        // Resolve config: explicit --config, else discover from the file's dir.
        let mut cfg = match &config_path {
            Some(p) => read_config(p),
            None => match Config::discover(&m1_lint::config::dir_of(path)) {
                Ok(c) => c,
                Err(e) => cfg_fail(e),
            },
        };
        if let Some(n) = max_line { cfg.max_line_length = n; }
        if let Some(n) = max_depth { cfg.max_nesting_depth = n; }
        if let Some(n) = max_complexity { cfg.max_complexity = n; }
        if let Err(e) = cfg.apply_filters(select.clone(), ignore.clone()) {
            cfg_fail(e);
        }

        let runner = Runner::new(Registry::from_config(&cfg));

        if do_fix {
            if let Err(e) = runner.fix_file(path) {
                eprintln!("warning: could not fix {}: {}", path.display(), e);
            }
        }

        match runner.run_file(path) {
            Ok(result) => {
                if !result.syntax_errors.is_empty() { any_error = true; }
                if result.diagnostics.iter().any(|d| d.inner.severity == m1_core::Severity::Error) {
                    any_error = true;
                }
                match format {
                    Format::Human => {
                        eprint!("{}", report::render_human(&path.display().to_string(), &result));
                    }
                    Format::Json => json_files.push((path.display().to_string(), result)),
                }
            }
            Err(e) => fail(&format!("could not read {}: {}", path.display(), e)),
        }
    }

    if let Format::Json = format {
        println!("{}", report::render_json(&json_files));
    }
    if any_error {
        process::exit(1);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    process::exit(2);
}
fn cfg_fail(e: m1_lint::config::ConfigError) -> ! {
    fail(&e.to_string())
}
fn req<'a>(v: Option<&'a String>, flag: &str) -> &'a str {
    v.map(String::as_str).unwrap_or_else(|| fail(&format!("{flag} requires a value")))
}
fn parse_num<T: std::str::FromStr>(v: Option<&String>, flag: &str) -> T {
    req(v, flag).parse().unwrap_or_else(|_| fail(&format!("{flag} expects a number")))
}
fn split_codes(v: Option<&String>, flag: &str) -> Vec<String> {
    req(v, flag).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}
fn read_config(p: &std::path::Path) -> Config {
    let text = match std::fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) => fail(&format!("could not read {}: {}", p.display(), e)),
    };
    match Config::from_toml_str(&text) {
        Ok(c) => c,
        Err(e) => cfg_fail(e),
    }
}
fn print_help() {
    println!("usage: m1-lint [OPTIONS] <file>...");
    println!();
    println!("OPTIONS:");
    println!("  --format <human|json>    output format (default: human)");
    println!("  --fix                    apply safe autofixes in place");
    println!("  --config <path>          use this .m1lint.toml");
    println!("  --max-line-length <N>");
    println!("  --max-nesting-depth <N>");
    println!("  --max-complexity <N>");
    println!("  --select <CODES>         comma-separated; only these rules run");
    println!("  --ignore <CODES>         comma-separated; remove these rules");
    println!("  -h, --help");
    println!("  -V, --version");
    println!();
    println!("--fix makes minimal edits; for full canonical formatting use m1-fmt.");
}
