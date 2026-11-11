//! Real-stdio lifecycle ordering and client round-trip regressions.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

struct Client {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
    id: u64,
    notifications: Vec<Value>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    fn new(capabilities: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_m1-lsp"))
            .arg("--stdio")
            .env("TOKIO_WORKER_THREADS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(s) = line.strip_prefix("Content-Length:") {
                        length = s.trim().parse::<usize>().ok();
                    }
                }
                let Some(length) = length else {
                    return;
                };
                let mut body = vec![0; length];
                if std::io::Read::read_exact(&mut reader, &mut body).is_err() {
                    return;
                }
                let Ok(message) = serde_json::from_slice(&body) else {
                    return;
                };
                if tx.send(message).is_err() {
                    return;
                }
            }
        });
        let mut client = Self {
            child,
            input,
            output,
            id: 0,
            notifications: Vec::new(),
        };
        let response = client.request(
            "initialize",
            json!({"capabilities":capabilities,"rootUri":null}),
        );
        assert!(response.get("error").is_none(), "{response}");
        client.notify("initialized", json!({}));
        client
    }

    fn send(&mut self, message: Value) {
        let text = message.to_string();
        write!(self.input, "Content-Length: {}\r\n\r\n{}", text.len(), text).unwrap();
        self.input.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let msg = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("request {method} did not complete"));
            if msg.get("method").is_some() {
                self.notifications.push(msg.clone());
            }
            if msg.get("method").is_some() && msg.get("id").is_some() {
                self.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
            } else if msg["id"] == id {
                return msg;
            }
        }
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"m1scr","version":1,"text":text}}),
        );
    }
}

#[test]
fn notifications_commit_in_wire_order_before_queries() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(tmp.path().join("Test.m1scr")).unwrap();
    let large: String = (0..2000)
        .map(|i| format!("local item{i} = {i};\n"))
        .collect();
    for mode in ["full", "ranges", "close_reopen", "invalid"] {
        let mut client = Client::new(json!({"textDocument":{"diagnostic":{}}}));
        client.open(uri.as_str(), &large);
        let (expected, count) = match mode {
            "full" => {
                client.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"local latest = 2;\n"}]}));
                ("latest", 1)
            }
            "ranges" | "invalid" => {
                if mode == "invalid" {
                    client.notify("textDocument/didChange", json!({}));
                    client.notify("workspace/didChangeWatchedFiles", json!({"changes":[]}));
                }
                for (version, end, name) in [(2, 11, "latest"), (3, 12, "final")] {
                    client.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"range":{"start":{"line":0,"character":6},"end":{"line":0,"character":end}},"text":name}]}));
                }
                ("final", 2000)
            }
            "close_reopen" => {
                client.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
                client.open(uri.as_str(), "local reopened = 1;\n");
                ("reopened", 1)
            }
            _ => unreachable!(),
        };
        // No settling delay: this request follows the notifications on the wire.
        let result = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        let symbols = result["result"].as_array().unwrap();
        assert_eq!(symbols.len(), count, "{mode}: {result}");
        assert_eq!(symbols[0]["name"], expected, "{mode}");
        assert_eq!(
            client.request("shutdown", Value::Null)["result"],
            Value::Null
        );
    }
}

#[test]
fn delayed_progress_acknowledgements_cannot_wedge_the_input_queue() {
    let mut client = Client::new(json!({"window":{"workDoneProgress":true}}));
    let mut remaining: std::collections::BTreeSet<u64> = (100..116).collect();
    for id in 100..116 {
        client.send(json!({"jsonrpc":"2.0","id":id,"method":"workspace/diagnostic","params":{"previousResultIds":[]}}));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut creates = Vec::new();
    while creates.len() < 16 {
        let msg = client
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("progress requests");
        if msg["method"] == "window/workDoneProgress/create" {
            creates.push(msg);
        } else if msg.get("method").is_some() && msg.get("id").is_some() {
            client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
        } else if let Some(id) = msg["id"].as_u64() {
            remaining.remove(&id);
        }
    }
    for _ in 0..130 {
        client.notify("$/unused", json!({}));
    }
    for msg in creates {
        client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
    }
    client.notify("$/cancelRequest", json!({"id":100}));
    let deadline = Instant::now() + Duration::from_secs(8);
    while !remaining.is_empty() {
        let msg = client
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| panic!("acknowledgement queue stalled; pending {remaining:?}"));
        if msg.get("method").is_some() && msg.get("id").is_some() {
            client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
        } else if let Some(id) = msg["id"].as_u64() {
            remaining.remove(&id);
        }
    }
    let healthy = client.request("workspace/symbol", json!({"query":""}));
    assert!(healthy.get("error").is_none(), "{healthy}");
}

fn next_publication(client: &mut Client, uri: &str, empty: bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let msg = client
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("diagnostic publication");
        if msg["method"] == "textDocument/publishDiagnostics"
            && msg["params"]["uri"] == uri
            && msg["params"]["diagnostics"].as_array().unwrap().is_empty() == empty
        {
            return msg["params"].clone();
        }
        if msg.get("method").is_some() && msg.get("id").is_some() {
            client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
        }
    }
}

#[test]
fn project_pushes_clear_on_failed_reload_and_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let prj = tmp.path().join("Project.m1prj");
    let xml = r#"<Project><Component Classname="BuiltIn.GroupCompound" Name="Root"/><Component Classname="BuiltIn.Parameter" Name="Root.Missing"><Props Type="u32"/></Component></Project>"#;
    std::fs::write(&prj, xml).unwrap();
    std::fs::write(
        tmp.path().join("parameters.m1cfg"),
        "<Configuration><Group Name=\"\"/></Configuration>",
    )
    .unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(&prj).unwrap();
    let script = tower_lsp::lsp_types::Url::from_file_path(tmp.path().join("Test.m1scr")).unwrap();
    let mut client = Client::new(json!({}));
    client.open(script.as_str(), "local x = 1;\n");
    assert!(
        !next_publication(&mut client, uri.as_str(), false)["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for action in ["corrupt", "restore", "delete"] {
        let change = match action {
            "corrupt" => {
                std::fs::write(&prj, "<bad").unwrap();
                2
            }
            "restore" => {
                std::fs::write(&prj, xml).unwrap();
                1
            }
            "delete" => {
                std::fs::remove_file(&prj).unwrap();
                3
            }
            _ => unreachable!(),
        };
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":uri,"type":change}]}),
        );
        let report = next_publication(&mut client, uri.as_str(), action != "restore");
        assert_eq!(
            report["diagnostics"].as_array().unwrap().is_empty(),
            action != "restore",
            "{action}: {report}"
        );
    }
}

#[test]
fn pull_sessions_never_receive_close_pushes() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(tmp.path().join("Test.m1scr")).unwrap();
    let mut client = Client::new(json!({"textDocument":{"diagnostic":{}}}));
    client.open(uri.as_str(), "local bad = ;\n");
    client.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
    client.request("workspace/symbol", json!({"query":""}));
    while let Ok(msg) = client.output.recv_timeout(Duration::from_millis(100)) {
        if msg.get("method").is_some() && msg.get("id").is_some() {
            client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
        }
        client.notifications.push(msg);
    }
    assert!(
        client
            .notifications
            .iter()
            .all(|m| m["method"] != "textDocument/publishDiagnostics"),
        "{:?}",
        client.notifications
    );
}

#[test]
fn rename_model_changes_only_after_client_applies_project_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let prj = tmp.path().join("Project.m1prj");
    let xml = r#"<Project><Component Classname="BuiltIn.GroupCompound" Name="Root"/><Component Classname="BuiltIn.Channel" Name="Root.Speed"><Props Type="f32"/></Component></Project>"#;
    std::fs::write(&prj, xml).unwrap();
    let project_uri = tower_lsp::lsp_types::Url::from_file_path(&prj).unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(tmp.path().join("Test.m1scr")).unwrap();
    let mut client = Client::new(json!({"textDocument":{"diagnostic":{}}}));
    client.open(uri.as_str(), "Root.Speed = 1;\n");
    let edit = client.request("textDocument/rename", json!({"textDocument":{"uri":uri},"position":{"line":0,"character":6},"newName":"Velocity"}));
    assert!(
        edit.get("error").is_none() && !edit["result"].is_null(),
        "{edit}"
    );
    let symbols = client.request("workspace/symbol", json!({"query":"Speed"}));
    assert_eq!(
        symbols["result"][0]["name"], "Root.Speed",
        "unapplied edit changed the model: {symbols}"
    );
    client.open(project_uri.as_str(), xml);
    client.notify("textDocument/didChange", json!({"textDocument":{"uri":project_uri,"version":2},"contentChanges":[{"text":xml.replace("Root.Speed", "Root.Velocity") }]}));
    let symbols = client.request("workspace/symbol", json!({"query":"Velocity"}));
    assert_eq!(
        symbols["result"][0]["name"], "Root.Velocity",
        "accepted edit did not refresh the model: {symbols}"
    );
    let added = tmp.path().join("Added.m1scr");
    std::fs::write(&added, "local x = 1;\n").unwrap();
    client.notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":tower_lsp::lsp_types::Url::from_file_path(added).unwrap(),"type":1}]}));
    let symbols = client.request("workspace/symbol", json!({"query":"Velocity"}));
    assert_eq!(
        symbols["result"][0]["name"], "Root.Velocity",
        "script discovery must preserve the unsaved project"
    );
    // Closing the unsaved buffer restores the actual on-disk project.
    client.notify(
        "textDocument/didClose",
        json!({"textDocument":{"uri":project_uri}}),
    );
    let symbols = client.request("workspace/symbol", json!({"query":"Speed"}));
    assert_eq!(symbols["result"][0]["name"], "Root.Speed");
}

#[test]
fn open_project_rediscovers_auxiliary_files() {
    let tmp = tempfile::tempdir().unwrap();
    let prj = tmp.path().join("Project.m1prj");
    let xml = r#"<Project><Component Classname="BuiltIn.GroupCompound" Name="Root"/><Component Classname="BuiltIn.Parameter" Name="Root.Missing"><Props Type="u32"/></Component></Project>"#;
    std::fs::write(&prj, xml).unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(&prj).unwrap();
    let mut client = Client::new(json!({"textDocument":{"diagnostic":{}}}));
    client.open(uri.as_str(), xml);
    let coverage = |client: &mut Client| {
        let report = client.request("workspace/diagnostic", json!({"previousResultIds":[]}));
        report["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| {
                file["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|d| d["code"] == "T041")
            })
    };
    assert!(!coverage(&mut client));
    for (name, present) in [
        ("parameters.m1cfg", true),
        ("parameters.m1cfg", false),
        ("replacement.m1cfg", true),
    ] {
        let cfg = tmp.path().join(name);
        if present {
            std::fs::write(&cfg, r#"<Configuration><Group Name=""/></Configuration>"#).unwrap();
        } else {
            std::fs::remove_file(&cfg).unwrap();
        }
        client.notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":tower_lsp::lsp_types::Url::from_file_path(&cfg).unwrap(),"type":if present {1} else {3}}]}));
        assert_eq!(coverage(&mut client), present, "{name} present={present}");
    }
    let dbc = tmp.path().join("added.m1dbc");
    for present in [true, false] {
        if present {
            std::fs::write(&dbc, r#"<DBC><ComponentStream><List><Component Classname="BuiltIn.CAN.DBC" Name="AddedBus"/></List></ComponentStream></DBC>"#).unwrap();
        } else {
            std::fs::remove_file(&dbc).unwrap();
        }
        client.notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":tower_lsp::lsp_types::Url::from_file_path(&dbc).unwrap(),"type":if present {1} else {3}}]}));
        let symbols = client.request("workspace/symbol", json!({"query":"AddedBus"}));
        assert_eq!(
            !symbols["result"].as_array().unwrap().is_empty(),
            present,
            "{symbols}"
        );
    }
}

#[test]
fn invalid_project_buffer_keeps_script_membership_current() {
    let tmp = tempfile::tempdir().unwrap();
    let prj = tmp.path().join("Project.m1prj");
    let xml = r#"<Project><Component Classname="BuiltIn.GroupCompound" Name="Root"/></Project>"#;
    std::fs::write(&prj, xml).unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(&prj).unwrap();
    let mut client = Client::new(json!({"textDocument":{"diagnostic":{}}}));
    client.open(uri.as_str(), &xml.replace("Root", "Unsaved"));
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"<bad"}]}),
    );
    let added = tmp.path().join("Added.m1scr");
    let added_uri = tower_lsp::lsp_types::Url::from_file_path(&added).unwrap();
    for present in [true, false] {
        if present {
            std::fs::write(&added, "local broken = ;\n").unwrap();
        } else {
            std::fs::remove_file(&added).unwrap();
        }
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":added_uri,"type":if present {1} else {3}}]}),
        );
        let report = client.request("workspace/diagnostic", json!({"previousResultIds":[]}));
        assert_eq!(
            report["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["uri"] == added_uri.as_str()),
            present,
            "{report}"
        );
        let symbols = client.request("workspace/symbol", json!({"query":"Unsaved"}));
        assert_eq!(
            symbols["result"][0]["name"], "Unsaved",
            "must retain last valid unsaved model"
        );
    }
}

#[test]
fn refreshes_resume_after_optional_request_saturation() {
    let mut client = Client::new(
        json!({"window":{"workDoneProgress":true},"textDocument":{"diagnostic":{}},"workspace":{"diagnostics":{"refreshSupport":true},"inlayHint":{"refreshSupport":true},"semanticTokens":{"refreshSupport":true},"codeLens":{"refreshSupport":true}}}),
    );
    let methods = [
        "workspace/diagnostic/refresh",
        "workspace/inlayHint/refresh",
        "workspace/semanticTokens/refresh",
        "workspace/codeLens/refresh",
    ];
    // Drain the initial refreshes before filling the permit budget.
    let mut expected: std::collections::BTreeSet<_> = methods.into_iter().collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !expected.is_empty() {
        let msg = client
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("initial refresh");
        if let Some(method) = msg["method"].as_str() {
            expected.remove(method);
        }
        if msg.get("method").is_some() && msg.get("id").is_some() {
            client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
        }
    }
    let mut remaining: std::collections::BTreeSet<u64> = (100..116).collect();
    for id in &remaining {
        client.send(json!({"jsonrpc":"2.0","id":id,"method":"workspace/diagnostic","params":{"previousResultIds":[]}}));
    }
    let mut creates = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while creates.len() < 16 || !remaining.is_empty() {
        let msg = client
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("progress timeout fallback");
        if msg["method"] == "window/workDoneProgress/create" {
            creates.push(msg);
        } else if let Some(id) = msg["id"].as_u64() {
            remaining.remove(&id);
        }
    }
    // Repeated mutations must coalesce while all permits remain held.
    for _ in 0..20 {
        client.notify("workspace/didChangeConfiguration", json!({"settings":{}}));
    }
    client.request("workspace/symbol", json!({"query":""}));
    for msg in creates {
        client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
    }
    let mut expected: std::collections::BTreeSet<_> = methods.into_iter().collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !expected.is_empty() {
        let msg = client
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| panic!("lost refreshes: {expected:?}"));
        if let Some(method) = msg["method"].as_str() {
            expected.remove(method);
        }
        if msg.get("method").is_some() && msg.get("id").is_some() {
            client.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":null}));
        }
    }
}
