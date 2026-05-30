use clap::Parser;
use m1_core::Severity;
use m1_typecheck::project::Project;
use m1_typecheck::rules::{check_script, check_script_no_project};
use std::path::PathBuf;
use std::process;

#[derive(Parser, Debug)]
#[command(name = "m1-typecheck", about = "Type checker for MoTeC M1 scripts")]
struct Args {
    /// Scripts to check
    files: Vec<PathBuf>,
    /// Project.m1prj (defaults to nearest upward, or $M1_PROJECT)
    #[arg(long)]
    project: Option<PathBuf>,
    /// parameters.m1cfg (optional)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Audit the project's own symbol names against the naming conventions (T050).
    #[arg(long)]
    audit_names: bool,
}

fn find_project(args: &Args) -> Option<PathBuf> {
    if let Some(p) = &args.project {
        return Some(p.clone());
    }
    if let Some(p) = std::env::var_os("M1_PROJECT") {
        return Some(PathBuf::from(p));
    }
    let start = args.files.first()?;
    let mut dir = start.parent()?;
    loop {
        let cand = dir.join("Project.m1prj");
        if cand.is_file() {
            return Some(cand);
        }
        dir = dir.parent()?;
    }
}

fn main() {
    let args = Args::parse();
    let mut had_error = false;

    let project_path = find_project(&args);
    let project = project_path.clone().map(|path| match Project::load(&path) {
        Ok(mut p) => {
            if let Some(cfg) = &args.config {
                p = p.with_config(cfg).unwrap_or_else(|e| {
                    eprintln!("m1-typecheck: config {}: {e}", cfg.display());
                    process::exit(2);
                });
            }
            p
        }
        Err(e) => {
            eprintln!("m1-typecheck: project {}: {e}", path.display());
            process::exit(2);
        }
    });
    if project.is_none() {
        eprintln!("m1-typecheck: no project found; running in project-less mode (T001 disabled)");
    }

    for file in &args.files {
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("m1-typecheck: {}: {e}", file.display());
                had_error = true;
                continue;
            }
        };
        let result = match &project {
            Some(p) => check_script(p, file, &src),
            None => check_script_no_project(&src),
        };
        for d in &result.syntax_errors {
            println!(
                "{}:{}:{}: error[syntax]: {}",
                file.display(),
                d.range.start.line + 1,
                d.range.start.column + 1,
                d.message
            );
            had_error = true;
        }
        for d in &result.diagnostics {
            let sev = match d.inner.severity {
                Severity::Error => {
                    had_error = true;
                    "error"
                }
                Severity::Warning => "warning",
                Severity::Info => "info",
                Severity::Hint => "hint",
            };
            println!(
                "{}:{}:{}: {}[{}]: {}",
                file.display(),
                d.inner.range.start.line + 1,
                d.inner.range.start.column + 1,
                sev,
                d.code.as_str(),
                d.inner.message
            );
        }
    }

    if args.audit_names {
        match &project {
            Some(p) => {
                let path_label = project_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<project>".into());
                for d in p.audit() {
                    println!(
                        "{}: {}[{}]: {}",
                        path_label,
                        match d.inner.severity {
                            Severity::Error => "error",
                            Severity::Warning => "warning",
                            Severity::Info => "info",
                            Severity::Hint => "hint",
                        },
                        d.code.as_str(),
                        d.inner.message
                    );
                }
            }
            None => eprintln!("m1-typecheck: --audit-names needs a project; skipping"),
        }
    }

    if had_error {
        process::exit(1);
    }
}
