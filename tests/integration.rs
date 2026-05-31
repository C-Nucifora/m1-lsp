use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tower_lsp::{LspService, Server};

// Helper: frame a JSON-RPC message with Content-Length, write it.
async fn write_msg(stream: &mut DuplexStream, msg: &Value) {
    let body = serde_json::to_string(msg).unwrap();
    let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    stream.write_all(framed.as_bytes()).await.unwrap();
}

async fn read_msg(stream: &mut DuplexStream) -> Value {
    // Read headers up to \r\n\r\n, parse Content-Length, then read the body.
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await.unwrap();
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = String::from_utf8(header).unwrap();
    let len: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn initialize_msg(id: i64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"initialize",
        "params":{"capabilities":{},"processId":null,"rootUri":null}})
}

#[tokio::test]
async fn initialize_advertises_capabilities() {
    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    // A single bidirectional duplex pair: the test drives `client`, the server
    // reads/writes its own `server` half (split into read+write halves).
    let (mut client, server) = duplex(8192);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });

    write_msg(&mut client, &initialize_msg(1)).await;
    let resp = read_msg(&mut client).await;
    let caps = &resp["result"]["capabilities"];
    assert_eq!(caps["documentFormattingProvider"], json!(true));
    assert_eq!(caps["textDocumentSync"], json!(1)); // FULL == 1
    assert_eq!(caps["hoverProvider"], json!(true));
    assert_eq!(caps["definitionProvider"], json!(true));
    assert_eq!(caps["documentSymbolProvider"], json!(true));
    assert!(caps.get("completionProvider").is_some());
}

// Direct-call tests of the pure analysis path (no transport needed).
#[test]
fn analyze_reports_syntax_error() {
    use m1_lsp::analysis::{NoLint, NoTypes, analyze};
    use m1_lsp::line_index::{LineIndex, PositionEncoding};
    use tower_lsp::lsp_types::Url;
    let src = "local <Integer> = 1;\n";
    let li = LineIndex::new(src);
    let uri = Url::parse("file:///x.m1scr").unwrap();
    let diags = analyze(&uri, src, &li, PositionEncoding::Utf16, &NoLint, &NoTypes);
    assert!(!diags.is_empty());
}
