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
    assert_eq!(caps["referencesProvider"], json!(true));
    assert_eq!(caps["documentHighlightProvider"], json!(true));
    assert_eq!(caps["foldingRangeProvider"], json!(true));
    assert_eq!(
        caps["codeActionProvider"]["codeActionKinds"],
        json!(["quickfix", "source.fixAll"])
    );
    assert_eq!(caps["documentSymbolProvider"], json!(true));
    assert!(caps.get("completionProvider").is_some());
    // `.` is registered so library-member completion auto-triggers.
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        json!(["."])
    );
}

async fn negotiate_encoding(encs: Value) -> String {
    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(8192);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
    let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "processId":null,"rootUri":null,
        "capabilities":{"general":{"positionEncodings":encs}}}});
    write_msg(&mut client, &msg).await;
    let resp = read_msg(&mut client).await;
    resp["result"]["capabilities"]["positionEncoding"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn position_encoding_respects_client_preference_order() {
    // The client's list is in preference order; pick the first we support.
    assert_eq!(
        negotiate_encoding(json!(["utf-16", "utf-8"])).await,
        "utf-16"
    );
    assert_eq!(
        negotiate_encoding(json!(["utf-8", "utf-16"])).await,
        "utf-8"
    );
    // Unsupported-first falls through to the next supported entry.
    assert_eq!(
        negotiate_encoding(json!(["utf-32", "utf-8"])).await,
        "utf-8"
    );
}

// Read messages until the response with `id` arrives, skipping the server's
// notifications (logMessage, publishDiagnostics, …) that interleave on the wire.
async fn read_response(stream: &mut DuplexStream, id: i64) -> Value {
    loop {
        let msg = read_msg(stream).await;
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return msg;
        }
    }
}

// #73: a client that opens a file without sending `rootUri`/workspace folders at
// `initialize` leaves the server project-less. Opening a `.m1scr` should then
// discover the project from the file itself. We prove it via `prepareRename` on a
// project leaf symbol: it reads the project store, so a non-null range means the
// project was loaded by the didOpen fallback (it would be null otherwise).
#[tokio::test]
async fn did_open_discovers_project_without_root_uri() {
    use std::io::Write;
    use tower_lsp::lsp_types::Url;

    let tmp = tempfile::tempdir().unwrap();
    let prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Parameter" Name="Root.Engine.Threshold"><Props Type="f32"/></Component>
</Project>"#;
    std::fs::File::create(tmp.path().join("Project.m1prj"))
        .unwrap()
        .write_all(prj.as_bytes())
        .unwrap();
    let script_uri = Url::from_file_path(tmp.path().join("Test.m1scr")).unwrap();
    let src = "Engine.Threshold = 1.0;\n";

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });

    // initialize with NO rootUri and NO workspace folders -> project-less.
    write_msg(&mut client, &initialize_msg(1)).await;
    let _ = read_response(&mut client, 1).await;

    // didOpen the script (notification).
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":script_uri,"languageId":"m1","version":1,"text":src}}}),
    )
    .await;

    // prepareRename on `Threshold` (line 0, char 8).
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/prepareRename","params":{
            "textDocument":{"uri":script_uri},
            "position":{"line":0,"character":8}}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    assert!(
        !resp["result"].is_null(),
        "prepareRename returned null — project was not loaded via the didOpen fallback: {resp}"
    );
}

// Read messages until a `textDocument/publishDiagnostics` for `uri` arrives,
// returning its diagnostics array. Bounded by a timeout so a missing publish
// fails the test instead of hanging.
async fn read_publish_for(stream: &mut DuplexStream, uri: &str) -> Vec<Value> {
    let fut = async {
        loop {
            let msg = read_msg(stream).await;
            if msg.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics")
                && msg["params"]["uri"] == json!(uri)
            {
                return msg["params"]["diagnostics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .unwrap_or_default()
}

// #139: project-level diagnostics (the `.m1cfg`-coverage audit, T041) must be
// surfaced in the editor, anchored to the `.m1prj`, once the project loads — not
// only on the CLI. A parameter declared in the project but missing from the
// `.m1cfg` should produce a T041 publishDiagnostics on the `.m1prj` URI.
#[tokio::test]
async fn project_diagnostics_published_for_m1prj_on_init() {
    use std::io::Write;
    use tower_lsp::lsp_types::Url;

    let tmp = tempfile::tempdir().unwrap();
    let prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Parameter" Name="Root.Engine.Covered"><Props Type="u32"/></Component>
  <Component Classname="BuiltIn.Parameter" Name="Root.Engine.Missing"><Props Type="u32"/></Component>
</Project>"#;
    std::fs::File::create(tmp.path().join("Project.m1prj"))
        .unwrap()
        .write_all(prj.as_bytes())
        .unwrap();
    std::fs::File::create(tmp.path().join("parameters.m1cfg"))
        .unwrap()
        .write_all(
            b"<?xml version=\"1.0\"?>\n<Configuration>\n <Group Name=\"\">\n\
              <Parameter Name=\"Engine.Covered\"><Cell Type=\"u32\"><![CDATA[1]]></Cell></Parameter>\n\
              </Group>\n</Configuration>",
        )
        .unwrap();

    let root_uri = Url::from_file_path(tmp.path()).unwrap();
    let prj_uri = Url::from_file_path(tmp.path().join("Project.m1prj")).unwrap();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });

    // initialize WITH rootUri so the project is loaded from it. Empty client
    // capabilities -> the server won't try dynamic watch registration (which
    // would otherwise await a client response).
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"capabilities":{},"processId":null,"rootUri":root_uri}}),
    )
    .await;
    let _ = read_response(&mut client, 1).await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await;

    let diags = read_publish_for(&mut client, prj_uri.as_str()).await;
    assert!(
        diags.iter().any(|d| d["code"] == json!("T041")
            && d["message"]
                .as_str()
                .map(|m| m.contains("Root.Engine.Missing"))
                .unwrap_or(false)),
        "expected a T041 project diagnostic for the uncovered parameter on the .m1prj; got {diags:?}"
    );
}

// #141: Go to Definition on a bare `local` returns its declaration in the same
// file, and works with no project loaded (locals are file-scoped).
#[tokio::test]
async fn goto_definition_resolves_a_local_without_project() {
    use tower_lsp::lsp_types::Url;

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
    write_msg(&mut client, &initialize_msg(1)).await; // rootUri: null -> project-less
    let _ = read_response(&mut client, 1).await;

    let uri = Url::parse("file:///tmp/Test.m1scr").unwrap();
    let src = "local myValue = 0;\nmyValue = myValue + 1;\n";
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":uri,"languageId":"m1","version":1,"text":src}}}),
    )
    .await;
    // Go to Definition on the use-site `myValue` (line 1, char 0).
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/definition","params":{
            "textDocument":{"uri":uri},"position":{"line":1,"character":0}}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    assert_eq!(
        resp["result"]["range"]["start"]["line"],
        json!(0),
        "goto on a local should return its declaration on line 0: {resp}"
    );
    assert_eq!(resp["result"]["uri"], json!(uri.as_str()));
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
    let diags = analyze(
        &uri,
        src,
        &li,
        PositionEncoding::Utf16,
        &NoLint,
        &NoTypes,
        &m1_lsp::config::DiagFilter::default(),
    );
    assert!(!diags.is_empty());
}
