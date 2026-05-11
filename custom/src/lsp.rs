use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use crossbeam_channel::{bounded, Receiver, Sender};

#[derive(Debug)]
pub struct Location {
    pub path: Box<str>,
    pub line: u32,      // 0-indexed
    pub col:  u32,      // 0-indexed, UTF-16 units (what LSP gives us)
}

// Handle to an in-flight async request.
// Drop it to abandon the request (response will be discarded by reader thread).
pub struct PendingRequest {
    rx: Receiver<Value>,
}

impl PendingRequest {
    // Returns Some(result) once the server has responded, None if still waiting.
    // Non-blocking - safe to call every frame.
    #[inline]
    pub fn poll(&self) -> Option<Value> {
        self.rx.try_recv().ok()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.rx.is_empty()
    }

    // Park the calling thread until done.
    // Only use this when you are okay blocking (e.g. startup, tests).
    #[inline]
    pub fn wait(self) -> Value {
        self.rx.recv().unwrap_or(Value::Null)
    }
}

pub type RequestId = u64;

pub struct LspClient(Option<LspClientInner>);

struct LspClientInner {
    stdin: ChildStdin,
    next_request_id: RequestId,

    // One entry per in-flight request. Reader thread removes the entry and
    // fires the sender when the matching response arrives.
    // Mutex only contended between main thread (insert) and reader (remove).
    pending: Arc<Mutex<HashMap<RequestId, Sender<Value>>>>,

    // Reused scratch buffer for outgoing JSON. Cleared before each send.
    // Avoids per-send allocation for file content (did_open, did_change).
    send_buf: Vec<u8>,

    // Keep child alive. Dropping it would close stdin and kill the server.
    _server_child: Child,

    pending_goto: Option<PendingRequest>,
}

impl LspClient {
    pub fn disabled() -> Self {
        Self(None)
    }

    // Spawn the LSP server process and send initialize/initialized.
    // Blocks until the server responds to initialize - this is the right
    // place to block because the editor isn't ready to use LSP yet anyway.
    pub fn start(server_cmd: &str, server_args: &[&str], workspace_root: &str) -> Self {
        let mut server_child = Command::new(server_cmd)
            .args(server_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to spawn LSP server");

        let stdout = server_child.stdout.take().unwrap();
        let stdin  = server_child.stdin.take().unwrap();

        let pending: Arc<Mutex<HashMap<RequestId, Sender<Value>>>> = Default::default();

        {
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || reader_thread(stdout, pending));
        }

        let mut inner = LspClientInner {
            stdin,
            next_request_id: 1,
            pending,
            send_buf: Vec::with_capacity(64 * 1024), // 64k initial, grows as needed
            _server_child: server_child,
            pending_goto: None,
        };

        inner.initialize(workspace_root);
        Self(Some(inner))
    }

    #[inline] fn inner(&mut self) -> Option<&mut LspClientInner> { self.0.as_mut() }

    // Writes the file text into `send_buf` with in-place JSON escaping
    pub fn did_open_buf(&mut self, path: &str, text: &str) {
        let Some(c) = self.inner() else { return };
        c.send_buf.clear();

        let uri  = file_uri(path);
        let lang = lang_id(path);

        // {
        //   "jsonrpc":"2.0",
        //   "method":"textDocument/didOpen",
        //   "params":
        //   {"textDocument":{"uri":"...","languageId":"...","version":1,"text":"<TEXT>"}}
        // }
        write!(c.send_buf,
               "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\",\"languageId\":\"{}\",\"version\":1,\"text\":",
               uri, lang
        ).unwrap();
        c.send_buf.push(b'"');
        write_json_string(text, &mut c.send_buf);
        c.send_buf.extend_from_slice(b"\"}}}"); // close: textDocument, params, root

        _ = flush_send_buf(&mut c.stdin, &c.send_buf);
    }

    #[inline]
    pub fn did_change_buf(&mut self, path: &str, text: &str, version: i32) {
        let Some(c) = self.inner() else { return };
        c.send_buf.clear();

        let uri = file_uri(path);

        // {
        //    "jsonrpc":"2.0",
        //     "method":"textDocument/didChange",
        //     "params": {"textDocument":{"uri":"...","version":N},"contentChanges":[{"text":"<TEXT>"}]}
        // }
        _ = write!(c.send_buf,
               "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\",\"version\":{}}},\"contentChanges\":[{{\"text\":",
               uri, version
        );
        c.send_buf.push(b'"');
        write_json_string(text, &mut c.send_buf);
        c.send_buf.extend_from_slice(b"\"}]}}"); // close: contentChanges obj, array, params, root

        _ = flush_send_buf(&mut c.stdin, &c.send_buf);
    }

    #[allow(unused)]
    #[inline]
    pub fn did_close(&mut self, path: &str) {
        let Some(c) = self.inner() else { return };
        c.notify("textDocument/didClose", json!({
            "textDocument": { "uri": file_uri(path) }
        }));
    }

    #[inline]
    pub fn goto_definition_async(
        &mut self,
        path:      &str,
        line:      u32,
        character: u32,
    ) {
        let Some(c) = self.inner() else { return };
        let rq = c.request_async("textDocument/definition", json!({
            "textDocument": { "uri": file_uri(path) },
            "position": { "line": line, "character": character }
        }));
        c.pending_goto = Some(rq);
    }

    #[inline]
    pub fn poll_goto_definition(&mut self) -> Option<Location> {
        let c = self.inner()?;
        let val = c.pending_goto.as_ref()?.poll()?;
        c.pending_goto = None;
        parse_location(val)
    }

    #[inline]
    pub fn goto_definition_is_some(&self) -> bool {
        self.0.as_ref()
            .and_then(|c| c.pending_goto.as_ref())
            .map_or(false, |g| g.is_empty())
    }

    // Call this before dropping the client.
    // Blocks on the shutdown response - that is correct per the LSP spec.
    #[inline]
    pub fn shutdown_blocking(&mut self) {
        let Some(c) = self.inner() else { return };
        let req = c.request_async("shutdown", json!(null));
        req.wait();
        c.notify("exit", json!(null));
    }
}

impl LspClientInner {
    #[inline]
    fn initialize(&mut self, root: &str) {
        let root_uri = format!("file://{root}");
        let req = self.request_async("initialize", json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "definition": { "dynamicRegistration": false }
                }
            }
        }));
        req.wait();  // Block here intentionally - can't use LSP until initialized
        self.notify("initialized", json!({}));
    }

    #[inline]
    fn request_async(&mut self, method: &str, params: Value) -> PendingRequest {
        let id = self.next_request_id;
        self.next_request_id += 1;

        let (tx, rx) = bounded(1);
        self.pending.lock().unwrap().insert(id, tx);

        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));

        PendingRequest { rx }
    }

    #[inline]
    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    // Generic send for small/structured messages. Allocates via serde_json.
    // For large payloads (file content) use the _buf variants above.
    #[inline]
    fn send(&mut self, msg: Value) {
        self.send_buf.clear();
        serde_json::to_writer(&mut self.send_buf, &msg).unwrap();
        _ = flush_send_buf(&mut self.stdin, &self.send_buf);
    }
}

//
// Sits on the server's stdout forever.
// Parses LSP Content-Length header + JSON body,
// Ignores notifications (@Incomplete).
//
fn reader_thread(stdout: ChildStdout, pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>) {
    let mut reader = BufReader::new(stdout);
    let mut line   = String::new();
    let mut body   = Vec::with_capacity(64 * 1024);

    loop {
        //
        // Read headers until blank line, pull out Content-Length.
        //
        let mut content_len = 0usize;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,  // Server closed stdout
                _ => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() { break; }
            if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
                content_len = val.parse().unwrap_or(0);
            }
        }

        if content_len == 0 { continue; }

        body.resize(content_len, 0);
        if std::io::Read::read_exact(&mut reader, &mut body).is_err() { return; }

        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };

        //
        // Only dispatch if it's a response (has "result" or "error") with an id.
        //
        let is_response = msg.get("result").is_some() || msg.get("error").is_some();
        let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else { continue };
        if !is_response { continue; }

        if let Some(err) = msg.get("error") {
            eprintln!("[lsp] error response id={id}: {err}");
        }

        let result = msg.get("result").cloned().unwrap_or(Value::Null);

        let mut map = pending.lock().unwrap();
        if let Some(tx) = map.remove(&id) {
            _ = tx.send(result);
        }
    }
}

// Write header + body to stdin. send_buf must contain only the body bytes.
// Separate function so we can call it from both send() and the _buf variants
// without borrowing issues.
#[inline]
fn flush_send_buf(stdin: &mut ChildStdin, body: &[u8]) -> std::io::Result<()> {
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(body)?;
    stdin.flush()
}

// Write `s` as a JSON string body (no surrounding quotes) into out.
// Escapes in-place, no allocation.
#[inline]
fn write_json_string(s: &str, out: &mut Vec<u8>) {
    for byte in s.bytes() {
        match byte {
            b'"'        => out.extend_from_slice(b"\\\""),
            b'\\'       => out.extend_from_slice(b"\\\\"),
            b'\n'       => out.extend_from_slice(b"\\n"),
            b'\r'       => out.extend_from_slice(b"\\r"),
            b'\t'       => out.extend_from_slice(b"\\t"),
            0x00..=0x1f => { write!(out, "\\u{:04x}", byte).unwrap(); }
            _           => out.push(byte),
        }
    }
}

#[inline]
fn file_uri(path: &str) -> String {
    format!("file://{path}")
}

#[inline]
fn lang_id(path: &str) -> &'static str {
    if      path.ends_with(".rs")                           { "rust"       }
    else if path.ends_with(".ts")  || path.ends_with(".tsx") { "typescript" }
    else if path.ends_with(".js")                           { "javascript" }
    else if path.ends_with(".c")   || path.ends_with(".h")   { "c"          }
    else if path.ends_with(".cpp") || path.ends_with(".cc") { "cpp"        }
    else                                                    { "plaintext"  }
}

// Result from LSP can be: Location | Location[] | LocationLink[] | null
#[inline]
fn parse_location(val: Value) -> Option<Location> {
    let loc = match &val {
        Value::Array(arr) => arr.first()?.clone(),
        Value::Object(_)  => val.clone(),
        _                 => return None,
    };

    let uri       = loc.get("uri")?.as_str()?;
    let path      = uri.strip_prefix("file://").unwrap_or(uri);
    let start     = loc.get("range")?.get("start")?;
    let line      = start.get("line")?.as_u64()? as u32;
    let character = start.get("character")?.as_u64()? as u32;

    Some(Location {
        path: path.into(),
        line,
        col: character,
    })
}
