//! CLI behaviour of the `m1-lsp` binary: `--help` / `--version` / unknown flags
//! must print and exit, never fall through to the stdio serve loop (#176).
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run the built `m1-lsp` binary with `args` and a hard timeout. `stdin` is
/// `null` (EOF) so that if a flag is wrongly ignored and the server starts, it
/// exits on EOF rather than hanging the test. Returns `(success, combined
/// stdout+stderr)`, or `None` if it had to be killed after the timeout.
fn run(args: &[&str], secs: u64) -> Option<(bool, String)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_m1-lsp"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn m1-lsp");
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = String::new();
            if let Some(mut so) = child.stdout.take() {
                let _ = so.read_to_string(&mut out);
            }
            let mut err = String::new();
            if let Some(mut se) = child.stderr.take() {
                let _ = se.read_to_string(&mut err);
            }
            return Some((status.success(), out + &err));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn version_flag_prints_and_exits() {
    let (ok, out) = run(&["--version"], 15).expect("--version must exit, not hang");
    assert!(ok, "--version should exit 0; got: {out}");
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "got: {out}");
    assert!(out.contains("m1-lsp"), "got: {out}");
}

#[test]
fn short_version_flag_works() {
    let (ok, out) = run(&["-V"], 15).expect("-V must exit, not hang");
    assert!(ok && out.contains(env!("CARGO_PKG_VERSION")), "got: {out}");
}

#[test]
fn help_flag_prints_usage_and_exits() {
    let (ok, out) = run(&["--help"], 15).expect("--help must exit, not hang");
    assert!(ok, "--help should exit 0; got: {out}");
    assert!(out.to_lowercase().contains("usage"), "got: {out}");
}

#[test]
fn unknown_flag_errors_and_exits() {
    let (ok, out) = run(&["--definitely-not-a-flag"], 15)
        .expect("an unknown flag must exit, not hang on stdin");
    assert!(!ok, "an unknown flag should exit non-zero; got: {out}");
    assert!(
        out.to_lowercase().contains("usage") || out.to_lowercase().contains("unknown"),
        "got: {out}"
    );
}

#[test]
fn stdio_flag_is_accepted_and_starts_the_server() {
    // vscode-languageclient with `TransportKind.stdio` spawns `m1-lsp --stdio`.
    // It must NOT be treated as an unknown option (which would print usage and
    // exit, breaking every VS Code session's server startup). With stdin at EOF
    // the started server exits cleanly, never printing the unknown-option error.
    let (ok, out) = run(&["--stdio"], 15).expect("--stdio must run the server, not hang");
    assert!(
        ok,
        "`--stdio` should start the server and exit on EOF; got: {out}"
    );
    assert!(
        !out.to_lowercase().contains("unknown option"),
        "`--stdio` must be accepted, not rejected; got: {out}"
    );
}

/// Frame a JSON-RPC message body with a Content-Length header (LSP base
/// protocol).
fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

/// Read framed LSP messages from `data` and return their decoded bodies. Used to
/// inspect the server's stdout in the shutdown test.
fn parse_framed(mut data: &[u8]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    while let Some(sep) = data.windows(4).position(|w| w == b"\r\n\r\n") {
        let header = std::str::from_utf8(&data[..sep]).unwrap_or("");
        let len: usize = header
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);
        let body_start = sep + 4;
        if body_start + len > data.len() {
            break;
        }
        if let Ok(v) = serde_json::from_slice(&data[body_start..body_start + len]) {
            out.push(v);
        }
        data = &data[body_start + len..];
    }
    out
}

/// Driving the real binary over stdio: a `shutdown` request carrying
/// `"params": null` (as neovim's `vim.lsp` and some vscode-languageclient
/// versions send) must return `result: null`, not JSON-RPC error -32602; the
/// following `exit` must then terminate the process cleanly (#292). Before the
/// fix tower-lsp 0.20 rejected the null params, so shutdown never took and the
/// process only ended on stdin EOF.
///
/// Like a real client we wait for the `initialize` *response* before sending
/// `shutdown`/`exit`. That ordering is not pedantry: tower-lsp's serve loop
/// dispatches each request as it reads it, and the `exit` handler cancels every
/// still-pending request — fire all four messages in a burst and `exit` can
/// cancel an `initialize` that hasn't finished, leaving the server uninitialised
/// (a race no spec-compliant client triggers).
#[test]
fn shutdown_with_null_params_succeeds_and_exits() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_m1-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn m1-lsp");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // Drain stdout on a background thread so a full pipe can never deadlock the
    // writes below; the shared buffer lets the main thread watch for responses.
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let collected = collected.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => collected.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        })
    };

    // Block until a response with `id` has been seen on stdout, or time out.
    let wait_for_id = |id: i64| -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let snapshot = collected.lock().unwrap().clone();
            if parse_framed(&snapshot)
                .iter()
                .any(|m| m["id"] == serde_json::json!(id))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    };

    stdin
        .write_all(&frame(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"processId":null,"rootUri":null}}"#,
        ))
        .unwrap();
    stdin.flush().unwrap();
    assert!(
        wait_for_id(1),
        "initialize must respond before we proceed (server stdout so far: {:?})",
        String::from_utf8_lossy(&collected.lock().unwrap())
    );

    stdin
        .write_all(&frame(
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        ))
        .unwrap();
    // The crux: paramless `shutdown` serialised with an explicit null.
    stdin
        .write_all(&frame(
            r#"{"jsonrpc":"2.0","id":18,"method":"shutdown","params":null}"#,
        ))
        .unwrap();
    stdin.flush().unwrap();
    assert!(wait_for_id(18), "shutdown must respond");

    stdin
        .write_all(&frame(r#"{"jsonrpc":"2.0","method":"exit"}"#))
        .unwrap();
    stdin.flush().unwrap();
    // Keep stdin open (don't drop it yet) so a clean exit is driven by the
    // protocol `exit`, not by stdin EOF — that's exactly the lingering #292 hit.

    let deadline = Instant::now() + Duration::from_secs(15);
    let exited = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Now release stdin and join the reader so we have the complete transcript.
    drop(stdin);
    let _ = reader.join();
    let out = collected.lock().unwrap().clone();

    assert!(
        exited.is_some(),
        "server must terminate after protocol `exit`, not hang until stdin EOF"
    );
    assert!(
        exited.unwrap().success(),
        "a clean shut-down then `exit` should exit 0"
    );

    let msgs = parse_framed(&out);
    let resp = msgs
        .iter()
        .find(|m| m["id"] == serde_json::json!(18))
        .unwrap_or_else(|| panic!("no shutdown response in: {msgs:?}"));
    assert!(
        resp.get("error").is_none(),
        "shutdown with null params must not error (-32602); got: {resp}"
    );
    assert!(
        resp.as_object().unwrap().contains_key("result"),
        "shutdown must return result: null; got: {resp}"
    );
    assert_eq!(resp["result"], serde_json::Value::Null);
}
