//! Pure-std synchronous HTTP/1.1 loopback protocol bridge.
//!
//! Accepts OpenAI Chat (`/v1/chat/completions`), Anthropic Messages
//! (`/v1/messages`), Gemini (`/v1beta/models/{model}:generateContent` and
//! `:streamGenerateContent`), and model-listing requests (`GET /v1/models`,
//! `GET /v1beta/models`, `GET /api/tags`), translating them to and from any
//! configured [`EndpointProtocol`] over [`HttpTransport`].

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use zeroize::{Zeroize, Zeroizing};

use crate::core::endpoints::{EndpointRecord, EndpointSecret};
use crate::core::error::{Error, Result};
use crate::core::protocol::{
    self, AuthScheme, EndpointProtocol, FetchedModel, HttpTransport, TransportError,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_AZURE_API_VERSION: &str = "2024-10-21";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(30);
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);

/// Upstream endpoint target configuration for the loopback bridge.
#[derive(Clone)]
pub struct BridgeTarget {
    pub base_url: String,
    pub protocol: EndpointProtocol,
    pub auth_scheme: AuthScheme,
    pub api_key: Zeroizing<String>,
    pub default_model: Option<String>,
    pub api_version: Option<String>,
    pub chat_path: Option<String>,
    pub models_path: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
    pub secret_headers: BTreeMap<String, String>,
    pub cached_models: Vec<FetchedModel>,
}

impl BridgeTarget {
    pub fn from_endpoint(
        record: &EndpointRecord,
        secret: &EndpointSecret,
        cached_models: Vec<FetchedModel>,
    ) -> Self {
        Self {
            base_url: record.base_url.clone(),
            protocol: record.effective_protocol(),
            auth_scheme: record.effective_auth_scheme(),
            api_key: secret.api_key.clone(),
            default_model: record.default_model.clone(),
            api_version: record.api_version.clone(),
            chat_path: record.chat_path.clone(),
            models_path: record.models_path.clone(),
            extra_headers: record.extra_headers.clone(),
            secret_headers: secret.secret_headers.clone(),
            cached_models,
        }
    }

    fn effective_protocol(&self) -> EndpointProtocol {
        if self.protocol == EndpointProtocol::Auto {
            EndpointProtocol::OpenAi
        } else {
            self.protocol
        }
    }

    fn secret_values(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if !self.api_key.is_empty() {
            out.push(self.api_key.as_str());
        }
        for v in self.secret_headers.values() {
            if !v.is_empty() {
                out.push(v.as_str());
            }
        }
        out
    }

    fn merged_headers(&self) -> BTreeMap<String, String> {
        let mut headers = self.extra_headers.clone();
        for (k, v) in &self.secret_headers {
            headers.insert(k.clone(), v.clone());
        }
        headers
    }
}

impl Drop for BridgeTarget {
    fn drop(&mut self) {
        self.api_key.zeroize();
        protocol::zeroize_string_map(&mut self.secret_headers);
    }
}

impl fmt::Debug for BridgeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeTarget")
            .field("base_url", &self.base_url)
            .field("protocol", &self.protocol)
            .field("auth_scheme", &self.auth_scheme)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("default_model", &self.default_model)
            .field("secret_header_count", &self.secret_headers.len())
            .finish()
    }
}

/// Handle to a running loopback HTTP/1.1 bridge. Stops cleanly when [`stop`](Self::stop)
/// is called or when dropped.
pub struct BridgeHandle {
    pub addr: SocketAddr,
    pub stop_flag: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl BridgeHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        // Nudge the listener in case it is mid-accept.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(100));
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a synchronous HTTP/1.1 bridge bound to `127.0.0.1:0`.
pub fn start_loopback_bridge(
    target: BridgeTarget,
    transport: Arc<dyn HttpTransport>,
) -> Result<BridgeHandle> {
    let addr: SocketAddr = "127.0.0.1:0"
        .parse()
        .map_err(|e| Error::backend(format!("invalid loopback addr: {e}")))?;
    start_bridge(addr, target, transport)
}

/// Start a synchronous HTTP/1.1 bridge bound to `bind_addr` (must be loopback).
pub fn start_bridge(
    bind_addr: SocketAddr,
    target: BridgeTarget,
    transport: Arc<dyn HttpTransport>,
) -> Result<BridgeHandle> {
    if !bind_addr.ip().is_loopback() {
        return Err(Error::refused(format!(
            "bridge bind address `{}` must be loopback",
            bind_addr.ip()
        )));
    }
    let listener = TcpListener::bind(bind_addr)
        .map_err(|e| Error::io(format!("binding bridge on {bind_addr}"), e))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Error::io("reading bridge local address", e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::io("configuring bridge listener", e))?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop_flag);
    let target = Arc::new(target);

    let thread = thread::Builder::new()
        .name("agyva-bridge".into())
        .spawn(move || {
            let mut active: Vec<JoinHandle<()>> = Vec::new();
            while !worker_stop.load(Ordering::SeqCst) {
                active.retain(|h| !h.is_finished());
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        if worker_stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let t = Arc::clone(&target);
                        let tr = Arc::clone(&transport);
                        if let Ok(h) = thread::Builder::new()
                            .name("agyva-bridge-conn".into())
                            .spawn(move || handle_connection(stream, &t, tr.as_ref()))
                        {
                            active.push(h);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            for h in active {
                let _ = h.join();
            }
        })
        .map_err(|e| Error::io("spawning bridge thread", e))?;

    Ok(BridgeHandle {
        addr,
        stop_flag,
        thread: Some(thread),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientKind {
    OpenAi,
    Anthropic,
    Gemini,
}

struct ParsedHttpRequest {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

fn handle_connection(mut stream: TcpStream, target: &BridgeTarget, transport: &dyn HttpTransport) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT));

    let req = match read_http_request(&mut stream) {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(msg) => {
            write_json_error(&mut stream, ClientKind::OpenAi, 400, &msg);
            return;
        }
    };

    if req.method == "OPTIONS" {
        let _ = write_http_response(
            &mut stream,
            204,
            "No Content",
            &[
                ("Access-Control-Allow-Origin", "*"),
                ("Access-Control-Allow-Headers", "*"),
            ],
            b"",
        );
        return;
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/models") => {
            handle_list_models(&mut stream, ClientKind::OpenAi, target, transport)
        }
        ("GET", "/v1beta/models") => {
            handle_list_models(&mut stream, ClientKind::Gemini, target, transport)
        }
        ("GET", "/api/tags") => handle_list_ollama_models(&mut stream, target, transport),
        ("POST", "/v1/chat/completions") => {
            handle_chat(
                &mut stream,
                ClientKind::OpenAi,
                None,
                false,
                &req.body,
                target,
                transport,
            );
        }
        ("POST", "/v1/messages") => {
            handle_chat(
                &mut stream,
                ClientKind::Anthropic,
                None,
                false,
                &req.body,
                target,
                transport,
            );
        }
        ("POST", p) if p.starts_with("/v1beta/models/") => {
            if let Some((model, stream_flag)) = parse_gemini_action(p, &req.query) {
                handle_chat(
                    &mut stream,
                    ClientKind::Gemini,
                    Some(&model),
                    stream_flag,
                    &req.body,
                    target,
                    transport,
                );
            } else {
                write_json_error(
                    &mut stream,
                    ClientKind::Gemini,
                    404,
                    "unsupported Gemini model action",
                );
            }
        }
        _ => {
            write_json_error(
                &mut stream,
                ClientKind::OpenAi,
                404,
                &format!("unknown route {} {}", req.method, req.path),
            );
        }
    }
}

fn parse_gemini_action(path: &str, query: &str) -> Option<(String, bool)> {
    let rest = path.strip_prefix("/v1beta/models/")?;
    let (model, action) = rest.rsplit_once(':')?;
    if model.is_empty() {
        return None;
    }
    let clean_model = model.strip_prefix("models/").unwrap_or(model).to_owned();
    let sse_query = query.split('&').any(|kv| kv == "alt=sse");
    match action {
        "generateContent" => Some((clean_model, sse_query)),
        "streamGenerateContent" => Some((clean_model, true)),
        _ => None,
    }
}

fn read_http_request(
    stream: &mut TcpStream,
) -> std::result::Result<Option<ParsedHttpRequest>, String> {
    let mut reader = BufReader::new(stream);
    let mut header_buf = Vec::new();
    let mut line = Vec::new();

    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("reading request headers: {e}"))?;
        if n == 0 {
            if header_buf.is_empty() {
                return Ok(None);
            }
            return Err("unexpected EOF in HTTP headers".into());
        }
        if header_buf.len() + n > MAX_HEADER_BYTES {
            return Err("HTTP headers exceed maximum size".into());
        }
        header_buf.extend_from_slice(&line);
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }

    let header_text = String::from_utf8(header_buf)
        .map_err(|_| "HTTP request headers are not valid UTF-8".to_owned())?;
    let mut lines = header_text.split('\n').map(|l| l.trim_end_matches('\r'));
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing HTTP method".to_owned())?
        .to_ascii_uppercase();
    let raw_target = parts
        .next()
        .ok_or_else(|| "missing HTTP request target".to_owned())?;

    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (raw_target.to_owned(), String::new()),
    };

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for hline in lines {
        if hline.is_empty() {
            continue;
        }
        if let Some((name, val)) = hline.split_once(':') {
            let lname = name.trim().to_ascii_lowercase();
            let lval = val.trim();
            if lname == "content-length" {
                let len = lval
                    .parse::<usize>()
                    .map_err(|_| "invalid Content-Length header".to_owned())?;
                if len > MAX_BODY_BYTES {
                    return Err("request body exceeds maximum size".into());
                }
                content_length = Some(len);
            } else if lname == "transfer-encoding" && lval.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }

    let body = if chunked {
        read_chunked_body(&mut reader)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("reading request body: {e}"))?;
        buf
    } else {
        Vec::new()
    };

    Ok(Some(ParsedHttpRequest {
        method,
        path,
        query,
        body,
    }))
}

fn read_chunked_body(
    reader: &mut BufReader<&mut TcpStream>,
) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("reading chunk size: {e}"))?;
        if n == 0 {
            return Err("unexpected EOF in chunked body".into());
        }
        let size_str = line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| format!("invalid chunk size `{size_str}`"))?;
        if size == 0 {
            line.clear();
            let _ = reader.read_line(&mut line);
            break;
        }
        if size > MAX_BODY_BYTES - out.len() {
            return Err("chunked request body exceeds maximum size".into());
        }
        let start = out.len();
        out.resize(start + size, 0);
        reader
            .read_exact(&mut out[start..])
            .map_err(|e| format!("reading chunk payload: {e}"))?;
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|e| format!("reading chunk terminator: {e}"))?;
    }
    Ok(out)
}

fn resolve_models_for_bridge(
    target: &BridgeTarget,
    transport: &dyn HttpTransport,
) -> Vec<FetchedModel> {
    if !target.cached_models.is_empty() {
        return target.cached_models.clone();
    }
    let headers = target.merged_headers();
    if let Ok(res) = protocol::fetch_models(
        transport,
        &target.base_url,
        target.protocol,
        Some(&target.auth_scheme),
        target.api_key.as_str(),
        target.api_version.as_deref(),
        target.models_path.as_deref(),
        &headers,
    ) && !res.models.is_empty()
    {
        return res.models;
    }
    if let Some(m) = target.default_model.as_deref() {
        return vec![FetchedModel {
            id: m.to_owned(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            owned_by: None,
            capabilities: Vec::new(),
        }];
    }
    Vec::new()
}

fn handle_list_models(
    stream: &mut TcpStream,
    client: ClientKind,
    target: &BridgeTarget,
    transport: &dyn HttpTransport,
) {
    let models = resolve_models_for_bridge(target, transport);
    let payload = match client {
        ClientKind::OpenAi | ClientKind::Anthropic => {
            let data: Vec<Value> = models
                .iter()
                .map(|m| {
                    json!({
                        "id": m.id,
                        "object": "model",
                        "owned_by": m.owned_by.as_deref().unwrap_or("custom"),
                    })
                })
                .collect();
            json!({ "object": "list", "data": data })
        }
        ClientKind::Gemini => {
            let list: Vec<Value> = models
                .iter()
                .map(|m| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("name".into(), Value::String(format!("models/{}", m.id)));
                    obj.insert(
                        "displayName".into(),
                        Value::String(m.display_name.as_deref().unwrap_or(&m.id).to_owned()),
                    );
                    if let Some(ctx) = m.context_window {
                        obj.insert("inputTokenLimit".into(), Value::from(ctx));
                    }
                    if let Some(max_out) = m.max_output_tokens {
                        obj.insert("outputTokenLimit".into(), Value::from(max_out));
                    }
                    obj.insert(
                        "supportedGenerationMethods".into(),
                        json!(["generateContent", "streamGenerateContent"]),
                    );
                    Value::Object(obj)
                })
                .collect();
            json!({ "models": list })
        }
    };
    write_json_ok(stream, &payload);
}

fn handle_list_ollama_models(
    stream: &mut TcpStream,
    target: &BridgeTarget,
    transport: &dyn HttpTransport,
) {
    let models = resolve_models_for_bridge(target, transport);
    let list: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "name": m.id,
                "model": m.id,
            })
        })
        .collect();
    write_json_ok(stream, &json!({ "models": list }));
}

#[derive(Debug, Clone)]
struct CanonicalMessage {
    role: String,
    text: String,
}

#[derive(Debug, Clone)]
struct CanonicalRequest {
    model: String,
    messages: Vec<CanonicalMessage>,
    system: Option<String>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    stream: bool,
}

#[derive(Debug, Clone)]
struct CanonicalResponse {
    id: String,
    model: String,
    text: String,
    finish_reason: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

fn handle_chat(
    stream: &mut TcpStream,
    client: ClientKind,
    path_model: Option<&str>,
    path_stream: bool,
    body: &[u8],
    target: &BridgeTarget,
    transport: &dyn HttpTransport,
) {
    let req_json: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            write_json_error(stream, client, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };

    let canonical = match parse_client_request(client, path_model, path_stream, &req_json, target) {
        Ok(c) => c,
        Err(msg) => {
            write_json_error(stream, client, 400, &msg);
            return;
        }
    };

    let upstream_proto = target.effective_protocol();
    let (url, headers, upstream_body) = encode_upstream_request(&canonical, target, upstream_proto);
    let body_bytes = match serde_json::to_vec(&upstream_body) {
        Ok(b) => b,
        Err(e) => {
            write_json_error(
                stream,
                client,
                500,
                &format!("failed to encode request: {e}"),
            );
            return;
        }
    };

    let secrets = target.secret_values();

    if canonical.stream {
        let resp = match transport.request_stream(
            "POST",
            &url,
            &headers,
            Some(&body_bytes),
            UPSTREAM_TIMEOUT,
        ) {
            Ok(r) => r,
            Err(e) => {
                let (status, msg) = map_transport_error(&e, &secrets);
                write_json_error(stream, client, status, &msg);
                return;
            }
        };

        if !(200..300).contains(&resp.status) {
            let mut err_buf = Vec::new();
            let _ = resp.body.take(64 * 1024).read_to_end(&mut err_buf);
            let msg = format_upstream_http_error(resp.status, &err_buf, &secrets);
            write_json_error(stream, client, resp.status, &msg);
            return;
        }

        stream_upstream_to_client(
            stream,
            client,
            upstream_proto,
            &canonical.model,
            &resp.headers,
            resp.body,
        );
    } else {
        let resp =
            match transport.request("POST", &url, &headers, Some(&body_bytes), UPSTREAM_TIMEOUT) {
                Ok(r) => r,
                Err(e) => {
                    let (status, msg) = map_transport_error(&e, &secrets);
                    write_json_error(stream, client, status, &msg);
                    return;
                }
            };

        if !(200..300).contains(&resp.status) {
            let msg = format_upstream_http_error(resp.status, &resp.body, &secrets);
            write_json_error(stream, client, resp.status, &msg);
            return;
        }

        let upstream_val: Value = match serde_json::from_slice(&resp.body) {
            Ok(v) => v,
            Err(e) => {
                let clean = protocol::sanitize_error_message(&e.to_string(), &secrets);
                write_json_error(
                    stream,
                    client,
                    502,
                    &format!("invalid JSON from upstream: {clean}"),
                );
                return;
            }
        };

        let decoded = decode_upstream_response(upstream_proto, &canonical.model, &upstream_val);
        let client_json = encode_client_response(client, &decoded);
        write_json_ok(stream, &client_json);
    }
}

fn parse_client_request(
    client: ClientKind,
    path_model: Option<&str>,
    path_stream: bool,
    json: &Value,
    target: &BridgeTarget,
) -> std::result::Result<CanonicalRequest, String> {
    let fallback_model = target
        .default_model
        .as_deref()
        .or_else(|| target.cached_models.first().map(|m| m.id.as_str()))
        .unwrap_or("default");

    match client {
        ClientKind::OpenAi => {
            let model = json
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback_model)
                .to_owned();
            let raw_msgs = json
                .get("messages")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing `messages` array".to_owned())?;
            let mut system_parts = Vec::new();
            let mut messages = Vec::new();
            for m in raw_msgs {
                let role = m
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_owned();
                let text = extract_text_content(m.get("content"));
                if role == "system" || role == "developer" {
                    if !text.is_empty() {
                        system_parts.push(text);
                    }
                } else {
                    messages.push(CanonicalMessage { role, text });
                }
            }
            let system = (!system_parts.is_empty()).then(|| system_parts.join("\n"));
            let temperature = json.get("temperature").and_then(Value::as_f64);
            let max_tokens = json
                .get("max_tokens")
                .or_else(|| json.get("max_completion_tokens"))
                .and_then(Value::as_u64);
            let stream = json.get("stream").and_then(Value::as_bool).unwrap_or(false);
            Ok(CanonicalRequest {
                model,
                messages,
                system,
                temperature,
                max_tokens,
                stream,
            })
        }
        ClientKind::Anthropic => {
            let model = json
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback_model)
                .to_owned();
            let system = json
                .get("system")
                .map(|v| extract_text_content(Some(v)))
                .filter(|s| !s.is_empty());
            let raw_msgs = json
                .get("messages")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing `messages` array".to_owned())?;
            let mut messages = Vec::with_capacity(raw_msgs.len());
            for m in raw_msgs {
                let role = m
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_owned();
                let text = extract_text_content(m.get("content"));
                messages.push(CanonicalMessage { role, text });
            }
            let temperature = json.get("temperature").and_then(Value::as_f64);
            let max_tokens = json.get("max_tokens").and_then(Value::as_u64);
            let stream = json.get("stream").and_then(Value::as_bool).unwrap_or(false);
            Ok(CanonicalRequest {
                model,
                messages,
                system,
                temperature,
                max_tokens,
                stream,
            })
        }
        ClientKind::Gemini => {
            let model = path_model
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback_model)
                .to_owned();
            let system = json
                .get("systemInstruction")
                .or_else(|| json.get("system_instruction"))
                .and_then(|v| v.get("parts"))
                .and_then(Value::as_array)
                .map(|parts| collect_parts_text(parts))
                .filter(|s| !s.is_empty());
            let raw_contents = json
                .get("contents")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing `contents` array".to_owned())?;
            let mut messages = Vec::with_capacity(raw_contents.len());
            for c in raw_contents {
                let role = match c.get("role").and_then(Value::as_str).unwrap_or("user") {
                    "model" => "assistant".to_owned(),
                    other => other.to_owned(),
                };
                let text = c
                    .get("parts")
                    .and_then(Value::as_array)
                    .map(|p| collect_parts_text(p))
                    .unwrap_or_default();
                messages.push(CanonicalMessage { role, text });
            }
            let gen_cfg = json
                .get("generationConfig")
                .or_else(|| json.get("generation_config"));
            let temperature = gen_cfg
                .and_then(|g| g.get("temperature"))
                .and_then(Value::as_f64);
            let max_tokens = gen_cfg
                .and_then(|g| {
                    g.get("maxOutputTokens")
                        .or_else(|| g.get("max_output_tokens"))
                })
                .and_then(Value::as_u64);
            let stream =
                path_stream || json.get("stream").and_then(Value::as_bool).unwrap_or(false);
            Ok(CanonicalRequest {
                model,
                messages,
                system,
                temperature,
                max_tokens,
                stream,
            })
        }
    }
}

fn extract_text_content(val: Option<&Value>) -> String {
    let Some(v) = val else {
        return String::new();
    };
    if let Some(s) = v.as_str() {
        return s.to_owned();
    }
    if let Some(arr) = v.as_array() {
        let mut out = String::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                out.push_str(s);
            } else if let Some(txt) = item.get("text").and_then(Value::as_str) {
                out.push_str(txt);
            }
        }
        return out;
    }
    String::new()
}

fn collect_parts_text(parts: &[Value]) -> String {
    let mut out = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            out.push_str(t);
        }
    }
    out
}

fn encode_upstream_request(
    req: &CanonicalRequest,
    target: &BridgeTarget,
    proto: EndpointProtocol,
) -> (String, Vec<(String, String)>, Value) {
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "Accept".into(),
            if req.stream {
                "text/event-stream".into()
            } else {
                "application/json".into()
            },
        ),
    ];
    target
        .auth_scheme
        .apply(&mut headers, target.api_key.as_str());
    let merged = target.merged_headers();
    if proto == EndpointProtocol::Anthropic
        && !merged
            .keys()
            .any(|k| k.eq_ignore_ascii_case("anthropic-version"))
    {
        headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
    }
    for (k, v) in merged {
        if !k.trim().is_empty() {
            headers.push((k, v));
        }
    }

    let custom_chat = target.chat_path.as_deref();
    match proto {
        EndpointProtocol::OpenAi | EndpointProtocol::Custom | EndpointProtocol::Auto => {
            let url = protocol::join_endpoint_url(
                &target.base_url,
                custom_chat.unwrap_or("/v1/chat/completions"),
            );
            let body = build_openai_chat_body(req);
            (url, headers, body)
        }
        EndpointProtocol::OpenAiResponses => {
            let url = protocol::join_endpoint_url(
                &target.base_url,
                custom_chat.unwrap_or("/v1/responses"),
            );
            let input: Vec<Value> = req
                .messages
                .iter()
                .map(|m| {
                    json!({
                        "role": m.role,
                        "content": m.text,
                    })
                })
                .collect();
            let mut body = json!({
                "model": req.model,
                "input": input,
                "stream": req.stream,
            });
            if let Some(sys) = &req.system {
                body["instructions"] = Value::String(sys.clone());
            }
            if let Some(temp) = req.temperature {
                body["temperature"] = json!(temp);
            }
            if let Some(max) = req.max_tokens {
                body["max_output_tokens"] = json!(max);
            }
            (url, headers, body)
        }
        EndpointProtocol::Anthropic => {
            let url = protocol::join_endpoint_url(
                &target.base_url,
                custom_chat.unwrap_or("/v1/messages"),
            );
            let msgs: Vec<Value> = req
                .messages
                .iter()
                .map(|m| {
                    let role = if m.role == "assistant" {
                        "assistant"
                    } else {
                        "user"
                    };
                    json!({
                        "role": role,
                        "content": m.text,
                    })
                })
                .collect();
            let mut body = json!({
                "model": req.model,
                "messages": msgs,
                "max_tokens": req.max_tokens.unwrap_or(4096),
                "stream": req.stream,
            });
            if let Some(sys) = &req.system {
                body["system"] = Value::String(sys.clone());
            }
            if let Some(temp) = req.temperature {
                body["temperature"] = json!(temp);
            }
            (url, headers, body)
        }
        EndpointProtocol::Gemini => {
            let clean_model = req.model.strip_prefix("models/").unwrap_or(&req.model);
            let default_path = if req.stream {
                format!("/v1beta/models/{clean_model}:streamGenerateContent?alt=sse")
            } else {
                format!("/v1beta/models/{clean_model}:generateContent")
            };
            let url =
                protocol::join_endpoint_url(&target.base_url, custom_chat.unwrap_or(&default_path));
            let contents: Vec<Value> = req
                .messages
                .iter()
                .map(|m| {
                    let role = if m.role == "assistant" {
                        "model"
                    } else {
                        "user"
                    };
                    json!({
                        "role": role,
                        "parts": [{"text": m.text}],
                    })
                })
                .collect();
            let mut body = json!({ "contents": contents });
            if let Some(sys) = &req.system {
                body["systemInstruction"] = json!({
                    "parts": [{"text": sys}]
                });
            }
            let mut gen_cfg = serde_json::Map::new();
            if let Some(temp) = req.temperature {
                gen_cfg.insert("temperature".into(), json!(temp));
            }
            if let Some(max) = req.max_tokens {
                gen_cfg.insert("maxOutputTokens".into(), json!(max));
            }
            if !gen_cfg.is_empty() {
                body["generationConfig"] = Value::Object(gen_cfg);
            }
            (url, headers, body)
        }
        EndpointProtocol::Ollama => {
            let url =
                protocol::join_ollama_url(&target.base_url, custom_chat.unwrap_or("/api/chat"));
            let mut msgs: Vec<Value> = Vec::new();
            if let Some(sys) = &req.system {
                msgs.push(json!({ "role": "system", "content": sys }));
            }
            for m in &req.messages {
                msgs.push(json!({ "role": m.role, "content": m.text }));
            }
            let mut body = json!({
                "model": req.model,
                "messages": msgs,
                "stream": req.stream,
            });
            let mut opts = serde_json::Map::new();
            if let Some(temp) = req.temperature {
                opts.insert("temperature".into(), json!(temp));
            }
            if let Some(max) = req.max_tokens {
                opts.insert("num_predict".into(), json!(max));
            }
            if !opts.is_empty() {
                body["options"] = Value::Object(opts);
            }
            (url, headers, body)
        }
        EndpointProtocol::AzureOpenAi => {
            let ver = target
                .api_version
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_AZURE_API_VERSION);
            let default_path = format!(
                "/openai/deployments/{}/chat/completions?api-version={ver}",
                req.model
            );
            let url =
                protocol::join_endpoint_url(&target.base_url, custom_chat.unwrap_or(&default_path));
            let body = build_openai_chat_body(req);
            (url, headers, body)
        }
        EndpointProtocol::Cohere => {
            let url =
                protocol::join_endpoint_url(&target.base_url, custom_chat.unwrap_or("/v2/chat"));
            let mut msgs: Vec<Value> = Vec::new();
            if let Some(sys) = &req.system {
                msgs.push(json!({ "role": "system", "content": sys }));
            }
            for m in &req.messages {
                msgs.push(json!({ "role": m.role, "content": m.text }));
            }
            let mut body = json!({
                "model": req.model,
                "messages": msgs,
                "stream": req.stream,
            });
            if let Some(temp) = req.temperature {
                body["temperature"] = json!(temp);
            }
            if let Some(max) = req.max_tokens {
                body["max_tokens"] = json!(max);
            }
            (url, headers, body)
        }
    }
}

fn build_openai_chat_body(req: &CanonicalRequest) -> Value {
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(sys) = &req.system {
        msgs.push(json!({ "role": "system", "content": sys }));
    }
    for m in &req.messages {
        msgs.push(json!({ "role": m.role, "content": m.text }));
    }
    let mut body = json!({
        "model": req.model,
        "messages": msgs,
        "stream": req.stream,
    });
    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = req.max_tokens {
        body["max_tokens"] = json!(max);
    }
    body
}

fn decode_upstream_response(
    proto: EndpointProtocol,
    fallback_model: &str,
    val: &Value,
) -> CanonicalResponse {
    let model = val
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(fallback_model)
        .to_owned();
    let id = val
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("agyva-bridge-1")
        .to_owned();

    match proto {
        EndpointProtocol::OpenAi
        | EndpointProtocol::AzureOpenAi
        | EndpointProtocol::Custom
        | EndpointProtocol::Auto => {
            let text = val
                .pointer("/choices/0/message/content")
                .and_then(|v| {
                    let s = extract_text_content(Some(v));
                    (!s.is_empty()).then_some(s)
                })
                .or_else(|| {
                    val.pointer("/choices/0/text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let finish_reason = val
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_owned();
            CanonicalResponse {
                id,
                model,
                text,
                finish_reason,
                input_tokens: val.pointer("/usage/prompt_tokens").and_then(Value::as_u64),
                output_tokens: val
                    .pointer("/usage/completion_tokens")
                    .and_then(Value::as_u64),
            }
        }
        EndpointProtocol::OpenAiResponses => {
            let text = val
                .get("output_text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    val.get("output")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.get("content").and_then(Value::as_array))
                                .flatten()
                                .filter_map(|c| c.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default()
                });
            CanonicalResponse {
                id,
                model,
                text,
                finish_reason: "stop".into(),
                input_tokens: val.pointer("/usage/input_tokens").and_then(Value::as_u64),
                output_tokens: val.pointer("/usage/output_tokens").and_then(Value::as_u64),
            }
        }
        EndpointProtocol::Anthropic => {
            let text = val
                .get("content")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            let finish_reason = match val.get("stop_reason").and_then(Value::as_str) {
                Some("max_tokens") => "length",
                _ => "stop",
            }
            .to_owned();
            CanonicalResponse {
                id,
                model,
                text,
                finish_reason,
                input_tokens: val.pointer("/usage/input_tokens").and_then(Value::as_u64),
                output_tokens: val.pointer("/usage/output_tokens").and_then(Value::as_u64),
            }
        }
        EndpointProtocol::Gemini => {
            let text = val
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .map(|p| collect_parts_text(p))
                .unwrap_or_default();
            let finish_reason = match val
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str)
            {
                Some("MAX_TOKENS") => "length",
                _ => "stop",
            }
            .to_owned();
            CanonicalResponse {
                id,
                model,
                text,
                finish_reason,
                input_tokens: val
                    .pointer("/usageMetadata/promptTokenCount")
                    .and_then(Value::as_u64),
                output_tokens: val
                    .pointer("/usageMetadata/candidatesTokenCount")
                    .and_then(Value::as_u64),
            }
        }
        EndpointProtocol::Ollama => {
            let text = val
                .pointer("/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let finish_reason = val
                .get("done_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_owned();
            CanonicalResponse {
                id,
                model,
                text,
                finish_reason,
                input_tokens: val.get("prompt_eval_count").and_then(Value::as_u64),
                output_tokens: val.get("eval_count").and_then(Value::as_u64),
            }
        }
        EndpointProtocol::Cohere => {
            let text = val
                .pointer("/message/content")
                .map(|v| extract_text_content(Some(v)))
                .or_else(|| val.get("text").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_default();
            let finish_reason = val
                .get("finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_owned();
            CanonicalResponse {
                id,
                model,
                text,
                finish_reason,
                input_tokens: val
                    .pointer("/usage/billed_units/input_tokens")
                    .and_then(Value::as_u64),
                output_tokens: val
                    .pointer("/usage/billed_units/output_tokens")
                    .and_then(Value::as_u64),
            }
        }
    }
}

fn encode_client_response(client: ClientKind, resp: &CanonicalResponse) -> Value {
    let in_tok = resp.input_tokens.unwrap_or(0);
    let out_tok = resp.output_tokens.unwrap_or(0);
    match client {
        ClientKind::OpenAi => json!({
            "id": resp.id,
            "object": "chat.completion",
            "created": 0,
            "model": resp.model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": resp.text,
                },
                "finish_reason": resp.finish_reason,
            }],
            "usage": {
                "prompt_tokens": in_tok,
                "completion_tokens": out_tok,
                "total_tokens": in_tok + out_tok,
            }
        }),
        ClientKind::Anthropic => {
            let stop_reason = if resp.finish_reason == "length" {
                "max_tokens"
            } else {
                "end_turn"
            };
            json!({
                "id": resp.id,
                "type": "message",
                "role": "assistant",
                "model": resp.model,
                "content": [{
                    "type": "text",
                    "text": resp.text,
                }],
                "stop_reason": stop_reason,
                "stop_sequence": Value::Null,
                "usage": {
                    "input_tokens": in_tok,
                    "output_tokens": out_tok,
                }
            })
        }
        ClientKind::Gemini => {
            let finish_reason = if resp.finish_reason == "length" {
                "MAX_TOKENS"
            } else {
                "STOP"
            };
            json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{ "text": resp.text }],
                    },
                    "finishReason": finish_reason,
                    "index": 0,
                }],
                "usageMetadata": {
                    "promptTokenCount": in_tok,
                    "candidatesTokenCount": out_tok,
                    "totalTokenCount": in_tok + out_tok,
                }
            })
        }
    }
}

fn stream_upstream_to_client(
    stream: &mut TcpStream,
    client: ClientKind,
    upstream_proto: EndpointProtocol,
    model: &str,
    upstream_headers: &[(String, String)],
    upstream_body: Box<dyn Read + Send>,
) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    if client == ClientKind::Anthropic {
        let start = json!({
            "type": "message_start",
            "message": {
                "id": "msg_agyva_bridge",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
            }
        });
        let block_start = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        });
        if write_sse_event(stream, Some("message_start"), &start).is_err()
            || write_sse_event(stream, Some("content_block_start"), &block_start).is_err()
        {
            return;
        }
    }

    let content_type = upstream_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    let is_json_fallback = content_type.contains("application/json")
        && !content_type.contains("event-stream")
        && upstream_proto != EndpointProtocol::Ollama;

    if is_json_fallback {
        let mut raw = Vec::new();
        if upstream_body
            .take(MAX_BODY_BYTES as u64)
            .read_to_end(&mut raw)
            .is_ok()
            && let Ok(val) = serde_json::from_slice::<Value>(&raw)
        {
            let decoded = decode_upstream_response(upstream_proto, model, &val);
            if !decoded.text.is_empty() {
                let _ = emit_client_stream_chunk(stream, client, model, &decoded.text);
            }
        }
        let _ = finish_client_stream(stream, client, model);
        return;
    }

    let mut reader = BufReader::new(upstream_body);
    let mut line = String::new();
    let mut buffered_json = String::new();
    let mut saw_sse_or_ndjson = false;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with(':') || trimmed.starts_with("event:") {
                    continue;
                }
                if let Some(data) = trimmed.strip_prefix("data:") {
                    saw_sse_or_ndjson = true;
                    let payload = data.trim();
                    if payload == "[DONE]" {
                        break;
                    }
                    if let Ok(val) = serde_json::from_str::<Value>(payload)
                        && let Some(chunk) = extract_stream_delta(upstream_proto, &val)
                        && !chunk.is_empty()
                        && emit_client_stream_chunk(stream, client, model, &chunk).is_err()
                    {
                        return;
                    }
                } else if upstream_proto == EndpointProtocol::Ollama {
                    saw_sse_or_ndjson = true;
                    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                        if let Some(chunk) = extract_stream_delta(EndpointProtocol::Ollama, &val)
                            && !chunk.is_empty()
                            && emit_client_stream_chunk(stream, client, model, &chunk).is_err()
                        {
                            return;
                        }
                        if val.get("done").and_then(Value::as_bool) == Some(true) {
                            break;
                        }
                    }
                } else if !saw_sse_or_ndjson {
                    if trimmed.len() <= MAX_BODY_BYTES - buffered_json.len() {
                        buffered_json.push_str(trimmed);
                    } else {
                        buffered_json.clear();
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Handle an upstream mock or endpoint that returned plain JSON without Content-Type.
    if !saw_sse_or_ndjson
        && !buffered_json.is_empty()
        && let Ok(val) = serde_json::from_str::<Value>(&buffered_json)
    {
        let decoded = decode_upstream_response(upstream_proto, model, &val);
        if !decoded.text.is_empty() {
            let _ = emit_client_stream_chunk(stream, client, model, &decoded.text);
        }
    }

    let _ = finish_client_stream(stream, client, model);
}

fn extract_stream_delta(proto: EndpointProtocol, val: &Value) -> Option<String> {
    match proto {
        EndpointProtocol::OpenAi
        | EndpointProtocol::AzureOpenAi
        | EndpointProtocol::Custom
        | EndpointProtocol::Auto => val
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .map(str::to_owned),
        EndpointProtocol::OpenAiResponses => {
            if val.get("type").and_then(Value::as_str) == Some("response.output_text.delta") {
                val.get("delta").and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            }
        }
        EndpointProtocol::Anthropic => val
            .pointer("/delta/text")
            .and_then(Value::as_str)
            .map(str::to_owned),
        EndpointProtocol::Gemini => val
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .map(|p| collect_parts_text(p)),
        EndpointProtocol::Ollama => val
            .pointer("/message/content")
            .and_then(Value::as_str)
            .map(str::to_owned),
        EndpointProtocol::Cohere => val
            .pointer("/delta/message/content/text")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn emit_client_stream_chunk(
    stream: &mut TcpStream,
    client: ClientKind,
    model: &str,
    text: &str,
) -> std::io::Result<()> {
    match client {
        ClientKind::OpenAi => {
            let ev = json!({
                "id": "chatcmpl-agyva-bridge",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "content": text },
                    "finish_reason": Value::Null,
                }]
            });
            write_sse_event(stream, None, &ev)
        }
        ClientKind::Anthropic => {
            let ev = json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "text_delta",
                    "text": text,
                }
            });
            write_sse_event(stream, Some("content_block_delta"), &ev)
        }
        ClientKind::Gemini => {
            let ev = json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{ "text": text }],
                    },
                    "index": 0,
                }]
            });
            write_sse_event(stream, None, &ev)
        }
    }
}

fn finish_client_stream(
    stream: &mut TcpStream,
    client: ClientKind,
    model: &str,
) -> std::io::Result<()> {
    match client {
        ClientKind::OpenAi => {
            let final_ev = json!({
                "id": "chatcmpl-agyva-bridge",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop",
                }]
            });
            write_sse_event(stream, None, &final_ev)?;
            stream.write_all(b"data: [DONE]\n\n")?;
            stream.flush()
        }
        ClientKind::Anthropic => {
            write_sse_event(
                stream,
                Some("content_block_stop"),
                &json!({"type": "content_block_stop", "index": 0}),
            )?;
            write_sse_event(
                stream,
                Some("message_delta"),
                &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            )?;
            write_sse_event(
                stream,
                Some("message_stop"),
                &json!({"type": "message_stop"}),
            )
        }
        ClientKind::Gemini => {
            let final_ev = json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{ "text": "" }],
                    },
                    "finishReason": "STOP",
                    "index": 0,
                }]
            });
            write_sse_event(stream, None, &final_ev)
        }
    }
}

fn write_sse_event(
    stream: &mut TcpStream,
    event: Option<&str>,
    data: &Value,
) -> std::io::Result<()> {
    let mut frame = String::new();
    if let Some(ev) = event {
        frame.push_str("event: ");
        frame.push_str(ev);
        frame.push('\n');
    }
    frame.push_str("data: ");
    frame.push_str(&serde_json::to_string(data).unwrap_or_else(|_| "{}".into()));
    frame.push_str("\n\n");
    stream.write_all(frame.as_bytes())?;
    stream.flush()
}

fn map_transport_error(err: &TransportError, secrets: &[&str]) -> (u16, String) {
    let status = match err {
        TransportError::Timeout(_) => 504,
        TransportError::Refused(_) => 400,
        TransportError::Network(_) | TransportError::TooLarge(_) => 502,
    };
    (
        status,
        protocol::sanitize_error_message(&err.to_string(), secrets),
    )
}

fn format_upstream_http_error(status: u16, body: &[u8], secrets: &[&str]) -> String {
    let raw = if let Ok(v) = serde_json::from_slice::<Value>(body)
        && let Some(msg) = v
            .pointer("/error/message")
            .or_else(|| v.pointer("/message"))
            .or_else(|| v.pointer("/error"))
            .and_then(Value::as_str)
    {
        msg.to_owned()
    } else {
        String::from_utf8_lossy(body).into_owned()
    };
    let clean = protocol::sanitize_error_message(raw.trim(), secrets);
    if clean.is_empty() {
        format!("upstream returned HTTP {status}")
    } else {
        format!("upstream HTTP {status}: {clean}")
    }
}

fn write_json_ok(stream: &mut TcpStream, payload: &Value) {
    let bytes = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let _ = write_http_response(
        stream,
        200,
        "OK",
        &[("Content-Type", "application/json; charset=utf-8")],
        &bytes,
    );
}

fn write_json_error(stream: &mut TcpStream, client: ClientKind, status: u16, message: &str) {
    let payload = match client {
        ClientKind::OpenAi => json!({
            "error": {
                "message": message,
                "type": "bridge_error",
                "code": status,
            }
        }),
        ClientKind::Anthropic => json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": message,
            }
        }),
        ClientKind::Gemini => json!({
            "error": {
                "code": status,
                "message": message,
                "status": "INTERNAL",
            }
        }),
    };
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    let _ = write_http_response(
        stream,
        status,
        reason_phrase(status),
        &[("Content-Type", "application/json; charset=utf-8")],
        &bytes,
    );
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}
