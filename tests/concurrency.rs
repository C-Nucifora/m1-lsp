//! Regression test for #336: concurrent requests must all complete
//! independently — a `workspace/diagnostic` scan must not stall the server.
//!
//! The pre-fix failure mode: tower-lsp polls every handler future inside the
//! single `serve` task (its `buffer_unordered` window), alongside the stdin
//! reader and the stdout writer. `block_in_place` in a handler therefore froze
//! the whole loop, and `Handle::block_on(progress_report(..))` *inside* that
//! frozen section deadlocked outright whenever the capacity-1 client channel
//! still held an undrained message (the `WorkDoneProgress::Begin` sent moments
//! before): the `$/progress` send parks until the stdout arm drains the
//! channel, and the stdout arm lives in the very task the handler is
//! blocking. `m1_lsp::concurrency::SpawnHandlers` (wired in `main.rs`)
//! removes the coupling by running each handler as its own tokio task.
//!
//! This test drives the **real binary over real stdio** deliberately: an
//! in-process duplex transport drains the client channel fast enough to dodge
//! the deadlock window, so only the production transport reproduces #336.
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn frame(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_string(msg).unwrap();
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

fn read_frame(r: &mut impl BufRead) -> Option<Value> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length: ") {
            len = v.trim().parse().ok();
        }
    }
    let mut body = vec![0u8; len?];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Answered-request ledger shared with the reader thread.
type Answered = Arc<Mutex<HashSet<i64>>>;

/// Read the server's stdout on a thread: record responses to our requests and
/// answer every server->client request with `null`, as a conforming editor
/// does (Neovim answers `workspace/diagnostic/refresh` and
/// `window/workDoneProgress/create`).
fn spawn_reader(child: &mut Child, stdin: Arc<Mutex<ChildStdin>>) -> Answered {
    let answered: Answered = Arc::default();
    let ledger = answered.clone();
    let stdout = child.stdout.take().expect("child stdout");
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        while let Some(msg) = read_frame(&mut r) {
            if msg.get("method").is_some() {
                if let Some(id) = msg.get("id") {
                    let reply = json!({"jsonrpc":"2.0","id":id,"result":null});
                    let mut w = stdin.lock().unwrap();
                    let _ = w.write_all(&frame(&reply));
                    let _ = w.flush();
                }
                continue; // notification or just-answered server request
            }
            if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                ledger.lock().unwrap().insert(id);
            }
        }
    });
    answered
}

fn send(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) {
    let mut w = stdin.lock().unwrap();
    w.write_all(&frame(msg)).unwrap();
    w.flush().unwrap();
}

fn wait_for(answered: &Answered, ids: &[i64], deadline: Instant) -> Vec<i64> {
    loop {
        let missing: Vec<i64> = {
            let got = answered.lock().unwrap();
            ids.iter().copied().filter(|i| !got.contains(i)).collect()
        };
        if missing.is_empty() || Instant::now() >= deadline {
            return missing;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A project big enough that the workspace scan reports progress at least once
/// (the report fires every 25 scripts) — the pre-fix deadlock trigger.
fn write_project(dir: &std::path::Path, scripts: usize) {
    let comps: String = (0..scripts)
        .map(|i| {
            format!(
                "  <Component Classname=\"BuiltIn.Channel\" Name=\"Root.Demo.Chan{i}\"><Props Type=\"f32\"/></Component>\n  \
                 <Component Classname=\"BuiltIn.FuncUser\" Filename=\"Demo.F{i}.m1scr\" Name=\"Root.Demo.F{i}\"/>\n"
            )
        })
        .collect();
    std::fs::write(
        dir.join("Project.m1prj"),
        format!(
            "<?xml version=\"1.0\"?>\n<MoTeCM1BuildSession>\n <Project Name=\"Big\" TargetHardware=\"ecu120\">\n  <ComponentStream><List>\n  \
             <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root.Demo\"/>\n{comps}  </List></ComponentStream>\n </Project>\n</MoTeCM1BuildSession>\n"
        ),
    )
    .unwrap();
    let sdir = dir.join("Scripts");
    std::fs::create_dir_all(&sdir).unwrap();
    for i in 0..scripts {
        std::fs::write(
            sdir.join(format!("Demo.F{i}.m1scr")),
            format!("local x = {i};\nChan{i} = x * 2.0;\n"),
        )
        .unwrap();
    }
}

// #336: a pull-diagnostics, progress-capable client (Neovim) fires
// `workspace/diagnostic` together with semantic-token requests, then opens a
// second buffer whose code-lens / diagnostic / semantic-token requests join
// the burst. Every request must be answered; before the fix the workspace
// scan deadlocked the whole server, leaving the second buffer's requests
// pending forever.
#[test]
fn concurrent_burst_with_workspace_scan_all_answered() {
    let tmp = tempfile::tempdir().unwrap();
    write_project(tmp.path(), 30);
    let s0 = tmp.path().join("Scripts/Demo.F0.m1scr");
    let s1 = tmp.path().join("Scripts/Demo.F1.m1scr");
    let uri = |p: &std::path::Path| format!("file://{}", p.display());

    let mut child = Command::new(env!("CARGO_BIN_EXE_m1-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn m1-lsp");
    let stdin = Arc::new(Mutex::new(child.stdin.take().expect("child stdin")));
    let answered = spawn_reader(&mut child, stdin.clone());

    // A Neovim-like capability set: pull diagnostics, semantic tokens with
    // range + full/delta, code lens, and window/workDoneProgress.
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "processId":null,"rootUri":uri(tmp.path()),
        "capabilities":{
            "textDocument":{
                "diagnostic":{},
                "semanticTokens":{"requests":{"range":true,"full":{"delta":true}},
                    "tokenTypes":[],"tokenModifiers":[],"formats":["relative"]},
                "codeLens":{}
            },
            "workspace":{"diagnostic":{}},
            "window":{"workDoneProgress":true}
        }}}),
    );
    let missing = wait_for(&answered, &[1], Instant::now() + Duration::from_secs(30));
    assert!(missing.is_empty(), "no initialize response");
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    );
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":uri(&s0),"languageId":"m1scr","version":1,
                "text":std::fs::read_to_string(&s0).unwrap()}}}),
    );
    // Let the post-open refresh round-trip settle, as a real editor session
    // does before the user opens the next buffer.
    std::thread::sleep(Duration::from_millis(500));

    // The burst from the issue, without waiting between requests.
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","id":2,"method":"workspace/diagnostic","params":{
            "identifier":"m1-lsp",
            "previousResultIds":[{"uri":uri(&s0),"value":"0"}]}}),
    );
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","id":3,"method":"textDocument/semanticTokens/range","params":{
            "textDocument":{"uri":uri(&s0)},
            "range":{"start":{"line":0,"character":0},"end":{"line":2,"character":0}}}}),
    );
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","id":4,"method":"textDocument/semanticTokens/full/delta","params":{
            "textDocument":{"uri":uri(&s0)},"previousResultId":"0"}}),
    );
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
            "textDocument":{"uri":uri(&s1),"languageId":"m1scr","version":1,
                "text":std::fs::read_to_string(&s1).unwrap()}}}),
    );
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","id":5,"method":"textDocument/codeLens","params":{
            "textDocument":{"uri":uri(&s1)}}}),
    );
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","id":6,"method":"textDocument/diagnostic","params":{
            "textDocument":{"uri":uri(&s1)}}}),
    );
    send(
        &stdin,
        &json!({"jsonrpc":"2.0","id":7,"method":"textDocument/semanticTokens/full","params":{
            "textDocument":{"uri":uri(&s1)}}}),
    );

    // Every request must complete; the generous deadline only trips when the
    // server has genuinely stalled (pre-fix: a permanent deadlock).
    let missing = wait_for(
        &answered,
        &[2, 3, 4, 5, 6, 7],
        Instant::now() + Duration::from_secs(30),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        missing.is_empty(),
        "server stalled (#336): requests {missing:?} were never answered"
    );
}
