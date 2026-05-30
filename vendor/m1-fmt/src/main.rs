use clap::Parser;
use std::path::PathBuf;
use std::process;

#[derive(Parser, Debug)]
#[command(name = "m1-fmt", about = "Autoformatter for MoTeC M1 scripts")]
struct Args {
    /// Files to format (reads from stdin if none given)
    files: Vec<PathBuf>,

    /// Check mode: exit 1 if any file would change, don't write
    #[arg(long)]
    check: bool,

    /// Write result to file in place
    #[arg(short = 'i', long = "in-place")]
    in_place: bool,

    /// Print a unified diff instead of formatted output
    #[arg(long)]
    diff: bool,

    /// Filename to use when reading from stdin
    #[arg(long, default_value = "<stdin>")]
    stdin_filename: String,

    /// Maximum consecutive blank lines to keep
    #[arg(long, default_value_t = 2)]
    max_blank_lines: usize,
}

/// Print a minimal unified diff between `original` and `formatted`.
fn print_diff(name: &str, original: &str, formatted: &str) {
    println!("--- {} (original)", name);
    println!("+++ {} (formatted)", name);
    let orig_lines: Vec<&str> = original.lines().collect();
    let fmt_lines: Vec<&str> = formatted.lines().collect();
    let max = orig_lines.len().max(fmt_lines.len());
    for i in 0..max {
        match (orig_lines.get(i), fmt_lines.get(i)) {
            (Some(o), Some(f)) if o == f => {}
            (Some(o), Some(f)) => {
                println!("-{}", o);
                println!("+{}", f);
            }
            (Some(o), None) => println!("-{}", o),
            (None, Some(f)) => println!("+{}", f),
            (None, None) => {}
        }
    }
}

fn main() {
    let args = Args::parse();
    let opts = m1_fmt::FormatOptions {
        max_blank_lines: args.max_blank_lines,
        ..Default::default()
    };
    let mut any_changed = false;
    let mut any_error = false;

    if args.files.is_empty() {
        // Read from stdin.
        let mut src = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut src).unwrap();
        match m1_fmt::format_str_with(&src, &opts) {
            Ok(result) => {
                for w in &result.warnings {
                    eprintln!("{}:{}: warning: {}", args.stdin_filename, w.line, w.message);
                }
                if args.diff {
                    if result.changed {
                        print_diff(&args.stdin_filename, &src, &result.output);
                    }
                } else if !args.check {
                    print!("{}", result.output);
                }
                if result.changed {
                    any_changed = true;
                    if args.check {
                        eprintln!("{}: would reformat", args.stdin_filename);
                    }
                }
            }
            Err(e) => {
                eprintln!("m1-fmt: {}: {}", args.stdin_filename, e);
                any_error = true;
            }
        }
    } else {
        for path in &args.files {
            let original = std::fs::read_to_string(path).ok();
            match m1_fmt::format_file_with(path, &opts) {
                Ok(result) => {
                    for w in &result.warnings {
                        eprintln!("{}:{}: warning: {}", path.display(), w.line, w.message);
                    }
                    if result.changed {
                        any_changed = true;
                        if args.check {
                            eprintln!("{}: would reformat", path.display());
                        } else if args.diff {
                            let orig = original.as_deref().unwrap_or("");
                            print_diff(&path.display().to_string(), orig, &result.output);
                        } else if args.in_place {
                            std::fs::write(path, &result.output).unwrap_or_else(|e| {
                                eprintln!("m1-fmt: {}: {}", path.display(), e);
                            });
                        } else {
                            print!("{}", result.output);
                        }
                    }
                }
                Err(m1_fmt::FormatError::SyntaxErrors(diags)) => {
                    eprintln!(
                        "m1-fmt: skipping {}: {} syntax error(s)",
                        path.display(),
                        diags.len()
                    );
                }
                Err(e) => {
                    eprintln!("m1-fmt: {}: {}", path.display(), e);
                    any_error = true;
                }
            }
        }
    }

    if any_error {
        process::exit(2);
    } else if any_changed && args.check {
        process::exit(1);
    }
}
