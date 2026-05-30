use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| {
        m1_lsp::backend::Backend::with_backends(
            client,
            Box::new(m1_lsp::lint_backend::M1Lint::new()),
            Box::new(m1_lsp::format::NoFormat), // replaced in Task 8
        )
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
