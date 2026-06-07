use std::sync::Arc;
use tower_lsp::{LspService, Server};

const USAGE: &str = "\
m1-lsp — Language Server for the MoTeC M1 script language (.m1scr)

Usage:
  m1-lsp                     Run the language server over stdio (the normal mode;
                             editors launch the binary with no arguments).
  m1-lsp --scaffold-config   Print a default m1-tools.toml and exit.
  m1-lsp --help, -h          Print this help and exit.
  m1-lsp --version, -V       Print the version and exit.
";

#[tokio::main]
async fn main() {
    // Handle CLI flags before touching stdio, so `--help`/`--version`/an unknown
    // flag print and exit instead of falling through to the blocking serve loop
    // (#176). Editors launch the binary with no arguments (stdio mode).
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--scaffold-config` prints a default `m1-tools.toml` and exits — the editors
    // invoke the bundled binary this way to generate the config file, so the
    // scaffold always matches the server's own tool versions.
    if args.iter().any(|a| a == "--scaffold-config") {
        print!("{}", m1_lsp::config::scaffold());
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("m1-lsp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // `--stdio` selects stdio transport — which is the only mode this server
    // supports. LSP clients append it automatically: vscode-languageclient with
    // `TransportKind.stdio` spawns `m1-lsp --stdio`. Accept it as a no-op (NOT an
    // unknown flag), otherwise the server would print usage and exit and every
    // VS Code session's server startup would fail.
    if let Some(bad) = args
        .iter()
        .find(|a| a.starts_with('-') && a.as_str() != "--stdio")
    {
        eprintln!("m1-lsp: unknown option `{bad}`\n\n{USAGE}");
        std::process::exit(2);
    }

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let store = Arc::new(m1_lsp::project_store::ProjectStore::new());
    let (service, socket) = LspService::new(move |client| {
        m1_lsp::backend::Backend::with_backends(
            client,
            Box::new(m1_lsp::lint_backend::M1Lint::new()),
            Box::new(m1_lsp::type_backend::M1Type::new(store.clone())),
            Box::new(m1_lsp::fmt_backend::M1Fmt::new()),
            store.clone(),
        )
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
