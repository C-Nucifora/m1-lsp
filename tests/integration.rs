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
    assert_eq!(caps["textDocumentSync"], json!(2)); // INCREMENTAL == 2 (#270)
    assert_eq!(caps["hoverProvider"], json!(true));
    assert_eq!(caps["definitionProvider"], json!(true));
    assert_eq!(caps["referencesProvider"], json!(true));
    assert_eq!(caps["documentHighlightProvider"], json!(true));
    assert_eq!(caps["foldingRangeProvider"], json!(true));
    assert_eq!(
        caps["codeActionProvider"]["codeActionKinds"],
        json!([
            "quickfix",
            "refactor.extract",
            "refactor.inline",
            "source.fixAll",
            "source"
        ])
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

#[tokio::test]
async fn code_action_offers_format_document_without_diagnostics() {
    use tower_lsp::lsp_types::Url;

    // A real formatter backend (Backend::new wires NoFormat, which never
    // reformats) so "Format Document" has edits to offer.
    let (service, socket) = LspService::new(|client| {
        m1_lsp::backend::Backend::with_backends(
            client,
            Box::new(m1_lsp::analysis::NoLint),
            Box::new(m1_lsp::analysis::NoTypes),
            Box::new(m1_lsp::fmt_backend::M1Fmt::new()),
            std::sync::Arc::new(m1_lsp::project_store::ProjectStore::new()),
        )
    });
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
    write_msg(&mut client, &initialize_msg(1)).await;
    let _ = read_response(&mut client, 1).await;

    // Unformatted but syntactically valid (K&R braces → reformats to Allman).
    let uri = Url::parse("file:///tmp/Fmt.m1scr").unwrap();
    let src = "if (a) {\nx = 1;\n}\n";
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":uri,"languageId":"m1","version":1,"text":src}}}),
    )
    .await;
    // Code-action request with NO diagnostics in context (#161).
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/codeAction","params":{
            "textDocument":{"uri":uri},
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
            "context":{"diagnostics":[]}}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    let titles: Vec<String> = resp["result"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["title"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        titles.iter().any(|t| t == "Format Document"),
        "expected a Format Document action with no diagnostics; got {titles:?}"
    );
}

// #140: pull diagnostics. The server must advertise `diagnosticProvider` so
// pull-capable clients (Neovim's vim.diagnostic, Helix) know it answers
// `textDocument/diagnostic` and `workspace/diagnostic`.
#[tokio::test]
async fn initialize_advertises_diagnostic_provider() {
    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
    write_msg(&mut client, &initialize_msg(1)).await;
    let resp = read_response(&mut client, 1).await;
    let dp = &resp["result"]["capabilities"]["diagnosticProvider"];
    assert_eq!(dp["interFileDependencies"], json!(false), "got {dp:?}");
    assert_eq!(dp["workspaceDiagnostics"], json!(true), "got {dp:?}");
    assert_eq!(dp["identifier"], json!("m1-lsp"), "got {dp:?}");
}

// #140: `textDocument/diagnostic` must run the analysis pass on demand for a
// file that has NOT been opened (read from disk), returning a full report.
// The diagnostic handlers wrap their blocking disk-read/analyze work in
// `block_in_place` (#258), which requires the multi-threaded runtime — the same
// flavor the production server uses (`#[tokio::main]`). The default
// current-thread test runtime would panic, so opt into multi_thread here.
#[tokio::test(flavor = "multi_thread")]
async fn pull_diagnostic_reports_findings_for_unopened_script() {
    use std::io::Write;
    use tower_lsp::lsp_types::Url;

    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::File::create(tmp.path().join("Project.m1prj"))
        .unwrap()
        .write_all(b"<?xml version=\"1.0\"?>\n<Project>\n  <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n</Project>")
        .unwrap();
    // `==` is a C operator M1 rejects -> always a `unsupported-c-token` ERROR,
    // independent of any project model, so it's a reliable pull-path signal.
    let script = scripts.join("Widget.m1scr");
    std::fs::write(&script, "local x = 0;\nx = a == b;\n").unwrap();

    let root_uri = Url::from_file_path(tmp.path()).unwrap();
    let script_uri = Url::from_file_path(&script).unwrap();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
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

    // Pull diagnostics for the script that was never opened.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/diagnostic","params":{
            "textDocument":{"uri":script_uri}}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    assert_eq!(resp["result"]["kind"], json!("full"), "got {resp}");
    let items = resp["result"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        items
            .iter()
            .any(|d| d["code"] == json!("unsupported-c-token")),
        "expected the unopened script's findings in the pull report; got {items:?}"
    );
}

// #140: `workspace/diagnostic` must aggregate findings across every script in
// the loaded project, including files that were never opened.
// Needs the multi-threaded runtime: the handler runs its collection loop under
// `block_in_place` (#258), matching the production `#[tokio::main]` server.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_diagnostic_covers_all_project_scripts() {
    use std::io::Write;
    use tower_lsp::lsp_types::Url;

    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::File::create(tmp.path().join("Project.m1prj"))
        .unwrap()
        .write_all(b"<?xml version=\"1.0\"?>\n<Project>\n  <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n</Project>")
        .unwrap();
    std::fs::write(scripts.join("Alpha.m1scr"), "local x = 0;\nx = a == b;\n").unwrap();
    std::fs::write(scripts.join("Beta.m1scr"), "local y = 0;\ny = c == d;\n").unwrap();

    let root_uri = Url::from_file_path(tmp.path()).unwrap();
    let alpha_uri = Url::from_file_path(scripts.join("Alpha.m1scr")).unwrap();
    let beta_uri = Url::from_file_path(scripts.join("Beta.m1scr")).unwrap();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
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

    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"workspace/diagnostic","params":{
            "previousResultIds":[]}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    let items = resp["result"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let uris: Vec<&str> = items.iter().filter_map(|r| r["uri"].as_str()).collect();
    assert!(
        uris.contains(&alpha_uri.as_str()) && uris.contains(&beta_uri.as_str()),
        "workspace report must cover every script; got {uris:?}"
    );
    // Each report carries that file's findings.
    for r in &items {
        if r["uri"] == json!(alpha_uri.as_str()) || r["uri"] == json!(beta_uri.as_str()) {
            let found = r["items"]
                .as_array()
                .map(|a| a.iter().any(|d| d["code"] == json!("unsupported-c-token")))
                .unwrap_or(false);
            assert!(found, "expected findings for {}; got {r}", r["uri"]);
        }
    }
}

// #259: a `textDocument/diagnostic` poll returns a `resultId`; re-polling with
// that `previousResultId` while nothing changed must return an `unchanged`
// report bearing the same id, instead of re-sending the full item set.
// The pull path now runs `diagnostics_for` under `block_in_place` (#258), which
// requires the multi-threaded runtime — the current-thread test runtime would
// panic, so opt into multi_thread here (same as the other pull-diagnostic tests).
#[tokio::test(flavor = "multi_thread")]
async fn pull_diagnostic_returns_unchanged_for_matching_result_id() {
    use std::io::Write;
    use tower_lsp::lsp_types::Url;

    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::File::create(tmp.path().join("Project.m1prj"))
        .unwrap()
        .write_all(b"<?xml version=\"1.0\"?>\n<Project>\n  <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n</Project>")
        .unwrap();
    let script = scripts.join("Widget.m1scr");
    std::fs::write(&script, "local x = 0;\nx = a == b;\n").unwrap();

    let root_uri = Url::from_file_path(tmp.path()).unwrap();
    let script_uri = Url::from_file_path(&script).unwrap();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
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

    // First poll: full report with a result id.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/diagnostic","params":{
            "textDocument":{"uri":script_uri}}}),
    )
    .await;
    let first = read_response(&mut client, 2).await;
    assert_eq!(first["result"]["kind"], json!("full"), "got {first}");
    let result_id = first["result"]["resultId"]
        .as_str()
        .expect("full report must carry a resultId")
        .to_string();

    // Second poll with that id, nothing changed: unchanged report, same id.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":3,"method":"textDocument/diagnostic","params":{
            "textDocument":{"uri":script_uri},"previousResultId":result_id}}),
    )
    .await;
    let second = read_response(&mut client, 3).await;
    assert_eq!(
        second["result"]["kind"],
        json!("unchanged"),
        "re-poll with a matching previousResultId must be unchanged; got {second}"
    );
    assert_eq!(
        second["result"]["resultId"],
        json!(result_id),
        "unchanged report must echo the stable result id; got {second}"
    );
}

// #281: `didChangeConfiguration` must refresh diagnostics for pull-diagnostics
// clients (VS Code). The handler used to only loop `publish`, which no-ops for
// pull clients, so a settings change stayed invisible until the next edit. After
// the fix it nudges pull clients via `workspace/diagnostic/refresh`, exactly like
// the watched-files (`.m1cfg`/config) path.
#[tokio::test(flavor = "multi_thread")]
async fn did_change_configuration_refreshes_pull_clients() {
    use std::io::Write;
    use tower_lsp::lsp_types::Url;

    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::File::create(tmp.path().join("Project.m1prj"))
        .unwrap()
        .write_all(b"<?xml version=\"1.0\"?>\n<Project>\n  <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n</Project>")
        .unwrap();
    let script = scripts.join("Widget.m1scr");
    std::fs::write(&script, "local x = 0;\nx = a == b;\n").unwrap();

    let root_uri = Url::from_file_path(tmp.path()).unwrap();
    let script_uri = Url::from_file_path(&script).unwrap();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
    // Initialize as a pull-diagnostics client: declare `textDocument.diagnostic`.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":null,"rootUri":root_uri,
            "capabilities":{"textDocument":{"diagnostic":{"dynamicRegistration":false}},
                "workspace":{"diagnostics":{"refreshSupport":true}}}}}),
    )
    .await;
    let _ = read_response(&mut client, 1).await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await;
    // Open a file producing a finding, then pull it so the client "holds" it.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":script_uri,"languageId":"m1","version":1,
                "text":"local x = 0;\nx = a == b;\n"}}}),
    )
    .await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/diagnostic","params":{
            "textDocument":{"uri":script_uri}}}),
    )
    .await;
    // Drain everything the open/initialize/pull path emitted (the server already
    // sends one `workspace/diagnostic/refresh` at `initialized`); answer any
    // server→client requests so the server doesn't stall, and stop once the
    // wire is quiet. From here on, anything new is caused by the config change.
    drain(&mut client).await;

    // A settings change. Before the fix the server emitted nothing here (no push
    // to a pull client, no refresh); after the fix it must request a refresh.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"workspace/didChangeConfiguration","params":{
            "settings":{"diagnostics":{"ignore":["L004"]}}}}),
    )
    .await;

    // The server must now send a fresh `workspace/diagnostic/refresh` request
    // (server→client) — this is the whole point of #281.
    let refresh = read_until_method(&mut client, "workspace/diagnostic/refresh").await;
    assert!(
        refresh.get("id").is_some(),
        "refresh must be a request the client can answer; got {refresh}"
    );
    let id = refresh["id"].clone();
    write_msg(&mut client, &json!({"jsonrpc":"2.0","id":id,"result":null})).await;
}

// #NNN: `didChangeWatchedFiles` for a created/deleted `.m1scr` must refresh
// diagnostics for pull-diagnostics clients (VS Code). Before the fix the handler
// called `refresh_scripts()` and returned early (the `!touches_project &&
// config_change.is_none()` guard), so pull clients never received a
// `workspace/diagnostic/refresh` and newly-created scripts stayed invisible until
// a manual re-pull.
#[tokio::test(flavor = "multi_thread")]
async fn scripts_changed_refreshes_pull_clients() {
    use std::io::Write;
    use tower_lsp::lsp_types::Url;

    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::File::create(tmp.path().join("Project.m1prj"))
        .unwrap()
        .write_all(b"<?xml version=\"1.0\"?>\n<Project>\n  <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n</Project>")
        .unwrap();
    let script = scripts.join("Widget.m1scr");
    std::fs::write(&script, "local x = 0;\nx = a == b;\n").unwrap();

    let root_uri = Url::from_file_path(tmp.path()).unwrap();
    let script_uri = Url::from_file_path(&script).unwrap();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });
    // Initialize as a pull-diagnostics client: declare `textDocument.diagnostic`.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":null,"rootUri":root_uri,
            "capabilities":{"textDocument":{"diagnostic":{"dynamicRegistration":false}},
                "workspace":{"diagnostics":{"refreshSupport":true}}}}}),
    )
    .await;
    let _ = read_response(&mut client, 1).await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await;
    // Open a file and drain all server traffic from initialization.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":script_uri,"languageId":"m1","version":1,
                "text":"local x = 0;\nx = a == b;\n"}}}),
    )
    .await;
    drain(&mut client).await;

    // Simulate a new `.m1scr` being created in the workspace.  This is a
    // watched-files notification for a `.m1scr` file only — it does NOT touch
    // a `.m1prj`/`.m1cfg` (so `touches_project` is false) and it is not a
    // config-file change (so `config_change` is None).  Before the fix the
    // handler returned early after `refresh_scripts()` and never sent a refresh.
    let new_script = scripts.join("NewScript.m1scr");
    std::fs::write(&new_script, "local y = 0;\n").unwrap();
    let new_script_uri = Url::from_file_path(&new_script).unwrap();
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{
            "changes":[{"uri":new_script_uri,"type":1}]}}),
    )
    .await;

    // The server must send a `workspace/diagnostic/refresh` so the pull client
    // re-pulls and sees the newly created script.
    let refresh = read_until_method(&mut client, "workspace/diagnostic/refresh").await;
    assert!(
        refresh.get("id").is_some(),
        "refresh must be a request the client can answer; got {refresh}"
    );
    let id = refresh["id"].clone();
    write_msg(&mut client, &json!({"jsonrpc":"2.0","id":id,"result":null})).await;
}

// Read messages until one with the given `method` arrives (skipping the server's
// interleaved notifications), answering any server→client requests so the server
// doesn't block on a pending response. Time-bounded so a missing message fails
// fast and deterministically (a pre-fix server that never refreshes would
// otherwise leave this read blocked forever).
async fn read_until_method(stream: &mut DuplexStream, method: &str) -> Value {
    let deadline = std::time::Duration::from_secs(3);
    loop {
        let msg = tokio::time::timeout(deadline, read_msg(stream))
            .await
            .unwrap_or_else(|_| panic!("did not observe a `{method}` message within {deadline:?}"));
        if msg.get("method").and_then(|m| m.as_str()) == Some(method) {
            return msg;
        }
        // Answer any other server request so it can make progress.
        if let Some(id) = msg.get("id")
            && msg.get("method").is_some()
        {
            write_msg(stream, &json!({"jsonrpc":"2.0","id":id,"result":null})).await;
        }
    }
}

// Drain pending server→client traffic until the wire goes quiet (a short read
// timeout elapses), answering any requests so the server doesn't stall.
async fn drain(stream: &mut DuplexStream) {
    while let Ok(msg) =
        tokio::time::timeout(std::time::Duration::from_millis(300), read_msg(stream)).await
    {
        if let Some(id) = msg.get("id")
            && msg.get("method").is_some()
        {
            write_msg(stream, &json!({"jsonrpc":"2.0","id":id,"result":null})).await;
        }
    }
}

// E4: a self-contained mini project (project + m1cfg + one script) written to a
// fresh tempdir, plus a scenario that feeds the input. Mirrors the in-tree
// `tests/fixtures/mini` so the LSP eval path runs the same deterministic project
// (Gain=2.5, scenario Speed=20 -> Output=50). Returns (tmpdir, root_uri,
// script_uri, script_src).
fn write_eval_fixture() -> (
    tempfile::TempDir,
    tower_lsp::lsp_types::Url,
    tower_lsp::lsp_types::Url,
    String,
) {
    use tower_lsp::lsp_types::Url;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Project.m1prj"),
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Mini" TargetHardware="ecu120">
  <ComponentStream><List>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Demo"/>
   <Component Classname="BuiltIn.Channel" Name="Root.Demo.Speed"><Props Type="f32"><Locale><Default Unit="rpm"/></Locale></Props></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.Demo.Output"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.Parameter" Name="Root.Demo.Gain"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="Demo.Update.m1scr" Name="Root.Demo.Update"/>
  </List></ComponentStream>
 </Project>
</MoTeCM1BuildSession>"#,
    )
    .unwrap();
    std::fs::write(
        root.join("parameters.m1cfg"),
        r#"<?xml version="1.0"?>
<Configuration Locale="English_Australia.1252" DefaultLocale="C">
 <Group Name="">
  <Parameter Name="Demo.Gain">
   <Cell Type="f32"><![CDATA[2.5]]></Cell>
  </Parameter>
 </Group>
</Configuration>"#,
    )
    .unwrap();
    let scripts = root.join("Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let src = "// Synthetic eval fixture script.\nlocal scaled = Speed * Gain;\nOutput = scaled;\n";
    let script = scripts.join("Demo.Update.m1scr");
    std::fs::write(&script, src).unwrap();
    // A scenario that drives Speed so Output = 20 * 2.5 = 50.
    std::fs::write(
        root.join("idle.toml"),
        "mode = \"function\"\ntarget = \"Demo.Update\"\n\
         duration_s = 0.03\nbase_rate_hz = 100.0\n\
         [[inputs]]\nchannel = \"Root.Demo.Speed\"\nconst = 20.0\n",
    )
    .unwrap();
    let root_uri = Url::from_file_path(root).unwrap();
    let script_uri = Url::from_file_path(&script).unwrap();
    (tmp, root_uri, script_uri, src.to_string())
}

// E4: with eval enabled and a scenario configured, hovering the `Output` channel
// shows the evaluated value alongside the existing type/symbol info. Needs the
// multi-threaded runtime: the eval-enabled hover path runs its trace build under
// `block_in_place`, matching the production `#[tokio::main(flavor)]` server.
#[tokio::test(flavor = "multi_thread")]
async fn hover_with_scenario_shows_evaluated_value() {
    let (_tmp, root_uri, script_uri, src) = write_eval_fixture();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });

    // Enable eval + point at the scenario via initializationOptions.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":null,"rootUri":root_uri,"capabilities":{},
            "initializationOptions":{"eval":{"enabled":true,"scenario":"idle.toml"}}}}),
    )
    .await;
    let _ = read_response(&mut client, 1).await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":script_uri,"languageId":"m1","version":1,"text":src}}}),
    )
    .await;
    drain(&mut client).await;

    // `Output` is the first token of the third line (0-indexed line 2, char 0).
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{
            "textDocument":{"uri":script_uri},
            "position":{"line":2,"character":0}}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    let md = resp["result"]["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("hover should return markup; got {resp}"));
    // The existing type/symbol info is still present.
    assert!(md.contains("channel"), "type/symbol info kept: {md}");
    // The evaluated value is appended (Output = 20 * 2.5 = 50).
    assert!(md.contains("value: `50`"), "evaluated value shown: {md}");
    // A configured scenario carries no offline-default honesty suffix.
    assert!(!md.contains("offline default"), "no offline label: {md}");
}

// E4 regression guard: with eval disabled (the default), the hover response is the
// pre-eval baseline — no `value:` line is ever added.
#[tokio::test(flavor = "multi_thread")]
async fn hover_with_eval_off_has_no_value_line() {
    let (_tmp, root_uri, script_uri, src) = write_eval_fixture();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });

    // No initializationOptions → eval stays disabled.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":null,"rootUri":root_uri,"capabilities":{}}}),
    )
    .await;
    let _ = read_response(&mut client, 1).await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":script_uri,"languageId":"m1","version":1,"text":src}}}),
    )
    .await;
    drain(&mut client).await;

    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{
            "textDocument":{"uri":script_uri},
            "position":{"line":2,"character":0}}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    let md = resp["result"]["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("hover should return markup; got {resp}"));
    assert!(md.contains("channel"), "type/symbol info present: {md}");
    assert!(!md.contains("value:"), "no value line when eval off: {md}");
}

// E5: hovering a sub-expression occurrence the run never recorded an expr value
// for (here the `scaled` local — `Trace::exprs` only carries call-site values, so
// this is a sparse miss) adds no `value:` line and leaves the rest of the hover
// intact. The buffer is unmodified-since-load, so the expr offsets are valid and
// the lookup actually runs — it simply finds nothing, the honest outcome. Drives
// the real backend so the buffer == disk gate is exercised end-to-end.
#[tokio::test(flavor = "multi_thread")]
async fn expr_hover_sparse_miss_leaves_hover_unchanged() {
    let (_tmp, root_uri, script_uri, src) = write_eval_fixture();

    let (service, socket) = LspService::new(m1_lsp::backend::Backend::new);
    let (mut client, server) = duplex(1 << 16);
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server);
        Server::new(r, w, socket).serve(service).await;
    });

    // Eval enabled + scenario configured (the expr lookup only runs when eval is on).
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":null,"rootUri":root_uri,"capabilities":{},
            "initializationOptions":{"eval":{"enabled":true,"scenario":"idle.toml"}}}}),
    )
    .await;
    let _ = read_response(&mut client, 1).await;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await;
    // Open the buffer with text identical to disk (unmodified-since-load): the E5
    // offset gate (`buffer_matches_disk`) is satisfied, so the expr lookup runs.
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":script_uri,"languageId":"m1","version":1,"text":src}}}),
    )
    .await;
    drain(&mut client).await;

    // Hover the `scaled` local on line 1 (`local scaled = Speed * Gain;`). It is a
    // non-call sub-expression, so the run recorded no expr value for it.
    let scaled_col = src.lines().nth(1).unwrap().find("scaled").unwrap() as u64;
    write_msg(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{
            "textDocument":{"uri":script_uri},
            "position":{"line":1,"character":scaled_col}}}),
    )
    .await;
    let resp = read_response(&mut client, 2).await;
    let md = resp["result"]["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("hover should return markup; got {resp}"));
    // The local's own symbol info is present; a sparse expr miss adds no value line.
    assert!(md.contains("`local`"), "local symbol info present: {md}");
    assert!(
        !md.contains("value:"),
        "a sparse expr miss adds no value line: {md}"
    );
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
