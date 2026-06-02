use std::sync::Arc;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    // `--scaffold-config` prints a default `m1-tools.toml` and exits — the editors
    // invoke the bundled binary this way to generate the config file, so the
    // scaffold always matches the server's own tool versions.
    if std::env::args().any(|a| a == "--scaffold-config") {
        print!("{}", m1_lsp::config::scaffold());
        return;
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
