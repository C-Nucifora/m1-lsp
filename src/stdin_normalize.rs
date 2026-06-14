//! stdin JSON-RPC normaliser for the LSP transport.
//!
//! tower-lsp 0.20 parses request `params` strictly: a paramless method
//! (`shutdown`, `exit`, …) that arrives with `"params": null` is rejected with
//! JSON-RPC error -32602 "Unexpected params: null" instead of running the
//! handler. LSP 3.17 §4.3 explicitly permits `params` to be `null` or omitted,
//! and several clients serialise no-parameter requests that way (neovim's
//! `vim.lsp`, some `vscode-languageclient` versions). The rejected `shutdown`
//! never moves the server into the shut-down state, so the following `exit` is
//! mishandled and the process lingers until stdin EOF (#292).
//!
//! Rather than fork/upgrade tower-lsp (the `"0.20"` pin is intentional — see
//! Cargo.toml and AGENTS.md), we sit a thin normaliser in front of it on the
//! transport: read Content-Length-framed messages off the real stdin, drop any
//! top-level `params` key whose value is `null` (semantically identical to an
//! absent key, and spec-permitted), re-frame with a corrected Content-Length,
//! and feed the result to the server. Non-JSON / unparseable bodies pass through
//! byte-for-byte so we never corrupt traffic we don't understand.
//!
//! The normaliser also makes the protocol `exit` actually terminate the
//! process. tower-lsp 0.20's serve loop runs `while framed_stdin.next().await`
//! and only ends when the input stream closes — its `exit` handler just flips an
//! internal flag, so after `exit` the loop blocks on the next read and the
//! process lingers until stdin EOF. Once we've forwarded `exit` we close the
//! pipe, which is the EOF the serve loop is waiting for: `serve` returns and the
//! process exits cleanly. (Real editors close their stdin after `exit`, so this
//! only matters for spec-compliant clients that rely on the protocol alone — but
//! that lingering is the second half of #292.)

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Remove a top-level `params` key whose value is `null` from a single
/// JSON-RPC message body, returning the re-serialised body. Anything else —
/// `params` with a real value, no `params` at all, a non-object, or invalid
/// JSON — is returned verbatim.
///
/// This is the whole semantic transform, kept pure so it can be unit-tested
/// without any I/O or async machinery.
pub fn strip_null_params(body: &str) -> String {
    // Only object messages can carry a `params` member; pass everything else
    // (arrays, scalars, invalid JSON) through unchanged.
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(body)
    else {
        return body.to_string();
    };
    match map.get("params") {
        // `"params": null` → drop the key. Absent and null are equivalent per
        // the spec, and tower-lsp 0.20 only accepts the absent form for
        // paramless methods.
        Some(serde_json::Value::Null) => {
            map.remove("params");
            // `Map` preserves insertion order (serde_json's default
            // `preserve_order` is off, but the order is irrelevant on the wire).
            serde_json::Value::Object(map).to_string()
        }
        // `params` present with a real value, or absent entirely: leave it be.
        // Round-tripping here would needlessly re-serialise valid traffic.
        _ => body.to_string(),
    }
}

/// Whether `body` is the LSP `exit` notification (`"method": "exit"`). Used by
/// [`pump`] to close the input pipe after forwarding `exit` so tower-lsp's serve
/// loop sees EOF and the process terminates (see the module docs). Anything that
/// isn't a JSON object with `method == "exit"` — including invalid JSON — is not
/// treated as `exit`, so we never close the pipe on traffic we don't understand.
fn is_exit_notification(body: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(body),
        Ok(serde_json::Value::Object(map))
            if map.get("method") == Some(&serde_json::Value::String("exit".to_string()))
    )
}

/// Read Content-Length-framed LSP messages from `input` (the real stdin),
/// normalise each body with [`strip_null_params`], re-frame with a corrected
/// `Content-Length`, and write the result to `output` (the pipe the server
/// reads). Returns on stdin EOF or the first I/O error, after which the caller
/// drops `output` to signal EOF to the server.
///
/// The framing is the LSP base protocol: one or more `Header: value\r\n` lines,
/// a blank `\r\n`, then exactly `Content-Length` bytes of UTF-8 body. We read
/// the body with `read_exact` so partial reads on the pipe can't truncate it.
pub async fn pump<R, W>(input: R, mut output: W) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(input);
    loop {
        // Read headers up to (and including) the blank line. An immediate EOF
        // here is a clean end of stream.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                // EOF. If we'd already started a message we just stop; tower-lsp
                // treats a closed input as shutdown.
                return Ok(());
            }
            // The header block ends at a blank line (`\r\n` or, defensively,
            // `\n`).
            if line == "\r\n" || line == "\n" {
                break;
            }
            // Parse `Content-Length: N`, case-insensitively on the header name.
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("Content-Length")
            {
                content_length = value.trim().parse::<usize>().ok();
            }
        }

        let Some(len) = content_length else {
            // A header block with no usable Content-Length: we can't frame a
            // body, so bail rather than guess. (A well-behaved client always
            // sends one; this guards against a malformed/truncated header block.)
            return Ok(());
        };

        // Read exactly `len` bytes of body.
        let mut body_bytes = vec![0u8; len];
        reader.read_exact(&mut body_bytes).await?;

        // Transform if the body is valid UTF-8; otherwise pass the raw bytes
        // through untouched (the spec mandates UTF-8, but we refuse to corrupt
        // anything we can't decode). Note whether this is the `exit`
        // notification so we can close the pipe once it's been forwarded.
        let (out_bytes, is_exit) = match std::str::from_utf8(&body_bytes) {
            Ok(body) => (
                strip_null_params(body).into_bytes(),
                is_exit_notification(body),
            ),
            Err(_) => (body_bytes, false),
        };

        // Re-frame with a Content-Length matching the (possibly shortened) body.
        let header = format!("Content-Length: {}\r\n\r\n", out_bytes.len());
        output.write_all(header.as_bytes()).await?;
        output.write_all(&out_bytes).await?;
        output.flush().await?;

        // `exit` was just forwarded: close the pipe (return → `output` dropped)
        // so tower-lsp's serve loop reads EOF and the process exits, instead of
        // blocking on the next read until stdin closes (#292).
        if is_exit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_params_key_is_removed() {
        let out =
            strip_null_params(r#"{"jsonrpc":"2.0","id":1,"method":"shutdown","params":null}"#);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            v.as_object().unwrap().get("params").is_none(),
            "null params must be removed, got: {out}"
        );
        // The rest of the message is preserved.
        assert_eq!(v["id"], serde_json::json!(1));
        assert_eq!(v["method"], serde_json::json!("shutdown"));
    }

    #[test]
    fn real_params_are_preserved() {
        let input = r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"processId":null}}"#;
        let out = strip_null_params(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // A nested `null` (processId) must NOT be touched — only a top-level
        // `params: null` is the target.
        assert_eq!(v["params"]["processId"], serde_json::Value::Null);
        assert!(v.as_object().unwrap().contains_key("params"));
    }

    #[test]
    fn missing_params_is_unchanged() {
        let input = r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#;
        // No `params` key at all: returned verbatim, including byte-for-byte.
        assert_eq!(strip_null_params(input), input);
    }

    #[test]
    fn invalid_json_is_returned_verbatim() {
        let input = "this is not json";
        assert_eq!(strip_null_params(input), input);
        let partial = r#"{"jsonrpc":"2.0","id":4,"method":"#;
        assert_eq!(strip_null_params(partial), partial);
    }

    #[test]
    fn non_object_json_is_returned_verbatim() {
        // A top-level array or scalar can't carry `params`; leave it alone.
        assert_eq!(strip_null_params("[1,2,3]"), "[1,2,3]");
        assert_eq!(strip_null_params("42"), "42");
        assert_eq!(strip_null_params("null"), "null");
    }

    #[test]
    fn exit_notification_is_recognised() {
        assert!(is_exit_notification(r#"{"jsonrpc":"2.0","method":"exit"}"#));
        // With an explicit null params (the #292 shape) it's still `exit`.
        assert!(is_exit_notification(
            r#"{"jsonrpc":"2.0","method":"exit","params":null}"#
        ));
    }

    #[test]
    fn non_exit_messages_are_not_exit() {
        // Other methods, responses, non-objects and invalid JSON must not be
        // mistaken for `exit` — closing the pipe on those would drop traffic.
        assert!(!is_exit_notification(
            r#"{"jsonrpc":"2.0","id":1,"method":"shutdown","params":null}"#
        ));
        assert!(!is_exit_notification(
            r#"{"jsonrpc":"2.0","result":null,"id":1}"#
        ));
        assert!(!is_exit_notification(r#"{"method":42}"#));
        assert!(!is_exit_notification("[\"exit\"]"));
        assert!(!is_exit_notification("not json"));
    }

    /// Drive [`pump`] over an in-process `duplex` pair: feed `input` as the
    /// reader's stdin (dropping the feeder so the read side hits EOF and `pump`
    /// returns), and collect everything `pump` writes to its output. Mirrors how
    /// `main.rs` wires `pump(stdin, norm_tx)` — stdin EOF is what ends the loop.
    async fn run_pump(input: &[u8]) -> Vec<u8> {
        // `duplex` is bidirectional: bytes written to `feeder` are readable from
        // `pump_in` (its peer). Feed the input, then drop `feeder` so `pump_in`
        // sees EOF — without that, `pump`'s `read_line` would block forever.
        let (mut feeder, pump_in) = tokio::io::duplex(8192);
        feeder.write_all(input).await.unwrap();
        drop(feeder);

        // `pump` writes to `pump_out`; we read the result from its peer, `sink`.
        let (pump_out, mut sink) = tokio::io::duplex(8192);
        pump(pump_in, pump_out).await.unwrap();
        // `pump` has returned, so its `pump_out` half is dropped and `sink` will
        // see EOF — `read_to_end` terminates.

        let mut buf = Vec::new();
        sink.read_to_end(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn pump_strips_null_params_and_reframes() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"shutdown","params":null}"#;
        let input = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        let out = run_pump(input.as_bytes()).await;
        let text = String::from_utf8(out).unwrap();
        let (header, payload) = text.split_once("\r\n\r\n").unwrap();
        let declared: usize = header
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The declared length must match the reframed body exactly.
        assert_eq!(declared, payload.len(), "Content-Length must be corrected");
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert!(v.as_object().unwrap().get("params").is_none());
    }

    #[tokio::test]
    async fn pump_passes_real_params_through() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"x":1}}"#;
        let input = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        let out = run_pump(input.as_bytes()).await;
        let text = String::from_utf8(out).unwrap();
        let (_header, payload) = text.split_once("\r\n\r\n").unwrap();
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(v["params"]["x"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn pump_stops_after_exit_and_drops_trailing_input() {
        // `exit` must end the pump so the server reads EOF and terminates. Any
        // message framed *after* `exit` must NOT be forwarded — a compliant
        // client sends nothing more, and tower-lsp is shutting down anyway.
        let exit = r#"{"jsonrpc":"2.0","method":"exit"}"#;
        let trailing = r#"{"jsonrpc":"2.0","id":99,"method":"after-exit"}"#;
        let input = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            exit.len(),
            exit,
            trailing.len(),
            trailing,
        );

        let out = run_pump(input.as_bytes()).await;
        let text = String::from_utf8(out).unwrap();
        // Exactly one forwarded message — the `exit` — and nothing after it.
        assert!(
            text.contains(r#""method":"exit""#),
            "exit must be forwarded: {text}"
        );
        assert!(
            !text.contains("after-exit"),
            "input after `exit` must be dropped: {text}"
        );
    }
}
