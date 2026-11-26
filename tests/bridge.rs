use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agyva::core::bridge::{BridgeTarget, start_loopback_bridge};
use agyva::core::protocol::{
    AuthScheme, EndpointProtocol, FetchedModel, HttpResponse, HttpTransport, TransportError,
    UreqTransport,
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

#[derive(Clone)]
struct UpstreamCall {
    body: Option<Value>,
}

type Handler = Box<
    dyn Fn(&str, &str, &[(String, String)], Option<&Value>) -> Result<HttpResponse, TransportError>
        + Send
        + Sync,
>;

struct MockUpstream {
    calls: Mutex<Vec<UpstreamCall>>,
    handler: Handler,
}

impl MockUpstream {
    fn new(
        handler: impl Fn(
            &str,
            &str,
            &[(String, String)],
            Option<&Value>,
        ) -> Result<HttpResponse, TransportError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            handler: Box::new(handler),
        }
    }

    fn take_calls(&self) -> Vec<UpstreamCall> {
        let mut g = self.calls.lock().unwrap();
        std::mem::take(&mut *g)
    }
}

impl HttpTransport for MockUpstream {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        let parsed = body.and_then(|b| serde_json::from_slice::<Value>(b).ok());
        self.calls.lock().unwrap().push(UpstreamCall {
            body: parsed.clone(),
        });
        (self.handler)(method, url, headers, parsed.as_ref())
    }
}

fn has_header(headers: &[(String, String)], name: &str, expected: &str) -> bool {
    headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(name) && v == expected)
}

fn client_post(url: &str, body: &Value) -> HttpResponse {
    let client = UreqTransport::new();
    let bytes = serde_json::to_vec(body).unwrap();
    client
        .request(
            "POST",
            url,
            &[("Content-Type".into(), "application/json".into())],
            Some(&bytes),
            Duration::from_secs(5),
        )
        .unwrap()
}

fn client_get(url: &str) -> HttpResponse {
    let client = UreqTransport::new();
    client
        .request("GET", url, &[], None, Duration::from_secs(5))
        .unwrap()
}

fn base_target(proto: EndpointProtocol, base_url: &str, auth: AuthScheme) -> BridgeTarget {
    BridgeTarget {
        base_url: base_url.into(),
        protocol: proto,
        auth_scheme: auth,
        api_key: Zeroizing::new("upstream-secret-key-xyz".into()),
        default_model: Some("default-model".into()),
        api_version: None,
        chat_path: None,
        models_path: None,
        extra_headers: BTreeMap::new(),
        secret_headers: BTreeMap::new(),
        local_token: None,
        cached_models: vec![FetchedModel {
            id: "test-model".into(),
            display_name: Some("Test Model".into()),
            context_window: Some(64000),
            max_output_tokens: Some(4096),
            owned_by: Some("acme".into()),
            capabilities: vec!["chat".into()],
        }],
    }
}

#[test]
fn gemini_client_to_openai_upstream_json_and_streaming() {
    let upstream = Arc::new(MockUpstream::new(|_, url, headers, body| {
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
        assert!(has_header(
            headers,
            "authorization",
            "Bearer upstream-secret-key-xyz"
        ));
        let b = body.unwrap();
        let stream = b.get("stream").and_then(Value::as_bool).unwrap_or(false);
        if stream {
            let sse = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"Gemini!\"}}]}\n\n",
                "data: [DONE]\n\n"
            );
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/event-stream".into())],
                body: sse.as_bytes().to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&json!({
                    "id": "chatcmpl-1",
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "Pong from OpenAI" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 11, "completion_tokens": 4 }
                }))
                .unwrap(),
            })
        }
    }));

    let target = base_target(
        EndpointProtocol::OpenAi,
        "https://api.openai.com/v1",
        AuthScheme::Bearer,
    );
    let mut handle = start_loopback_bridge(target, upstream.clone()).unwrap();
    let base = handle.base_url();

    // Non-streaming Gemini generateContent -> OpenAI
    let resp = client_post(
        &format!("{base}/v1beta/models/gpt-4o:generateContent"),
        &json!({
            "systemInstruction": { "parts": [{ "text": "Be brief." }] },
            "contents": [{ "role": "user", "parts": [{ "text": "Ping" }] }],
            "generationConfig": { "temperature": 0.2, "maxOutputTokens": 128 }
        }),
    );
    assert_eq!(resp.status, 200);
    let v: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(
        v["candidates"][0]["content"]["parts"][0]["text"],
        "Pong from OpenAI"
    );
    assert_eq!(v["candidates"][0]["finishReason"], "STOP");
    assert_eq!(v["usageMetadata"]["promptTokenCount"], 11);

    let calls = upstream.take_calls();
    assert_eq!(calls[0].body.as_ref().unwrap()["model"], "gpt-4o");
    assert_eq!(
        calls[0].body.as_ref().unwrap()["messages"][0]["content"],
        "Be brief."
    );
    assert_eq!(
        calls[0].body.as_ref().unwrap()["messages"][1]["content"],
        "Ping"
    );

    // Streaming Gemini streamGenerateContent -> OpenAI SSE
    let sse_resp = client_post(
        &format!("{base}/v1beta/models/gpt-4o:streamGenerateContent?alt=sse"),
        &json!({
            "contents": [{ "role": "user", "parts": [{ "text": "Say hi" }] }]
        }),
    );
    assert_eq!(sse_resp.status, 200);
    let sse_text = String::from_utf8(sse_resp.body).unwrap();
    assert!(sse_text.contains("\"Hello \""));
    assert!(sse_text.contains("\"Gemini!\""));
    assert!(sse_text.contains("\"finishReason\":\"STOP\""));

    handle.stop();
}

#[test]
fn anthropic_client_to_openai_upstream_json_and_streaming() {
    let upstream = Arc::new(MockUpstream::new(|_, url, _, body| {
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
        let stream = body
            .unwrap()
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if stream {
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Streamed reply\"}}]}\n\ndata: [DONE]\n\n";
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/event-stream".into())],
                body: sse.as_bytes().to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&json!({
                    "id": "cmpl-2",
                    "model": "gpt-4o",
                    "choices": [{
                        "message": { "role": "assistant", "content": "Hello Claude client" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 3 }
                }))
                .unwrap(),
            })
        }
    }));

    let target = base_target(
        EndpointProtocol::OpenAi,
        "https://api.openai.com/v1",
        AuthScheme::Bearer,
    );
    let mut handle = start_loopback_bridge(target, upstream).unwrap();
    let base = handle.base_url();

    let resp = client_post(
        &format!("{base}/v1/messages"),
        &json!({
            "model": "gpt-4o",
            "max_tokens": 256,
            "system": "System prompt",
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    assert_eq!(resp.status, 200);
    let v: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["content"][0]["text"], "Hello Claude client");
    assert_eq!(v["stop_reason"], "end_turn");

    let sse_resp = client_post(
        &format!("{base}/v1/messages"),
        &json!({
            "model": "gpt-4o",
            "max_tokens": 256,
            "stream": true,
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    assert_eq!(sse_resp.status, 200);
    let sse_text = String::from_utf8(sse_resp.body).unwrap();
    assert!(sse_text.contains("event: message_start"));
    assert!(sse_text.contains("event: content_block_delta"));
    assert!(sse_text.contains("Streamed reply"));
    assert!(sse_text.contains("event: message_stop"));

    handle.stop();
}

#[test]
fn openai_client_to_gemini_anthropic_and_ollama_upstreams() {
    // 1. OpenAI -> Gemini (JSON + SSE)
    let gem_upstream = Arc::new(MockUpstream::new(|_, url, headers, _| {
        assert!(has_header(
            headers,
            "x-goog-api-key",
            "upstream-secret-key-xyz"
        ));
        if url.contains(":streamGenerateContent") {
            let sse =
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Gem chunk\"}]}}]}\n\n";
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/event-stream".into())],
                body: sse.as_bytes().to_vec(),
            })
        } else {
            assert_eq!(
                url,
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
            );
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": [{ "text": "From Gemini" }] },
                        "finishReason": "STOP"
                    }],
                    "usageMetadata": { "promptTokenCount": 7, "candidatesTokenCount": 2 }
                }))
                .unwrap(),
            })
        }
    }));
    let mut gem_handle = start_loopback_bridge(
        base_target(
            EndpointProtocol::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            AuthScheme::XGoogApiKey,
        ),
        gem_upstream,
    )
    .unwrap();
    let resp = client_post(
        &format!("{}/v1/chat/completions", gem_handle.base_url()),
        &json!({
            "model": "gemini-2.5-flash",
            "messages": [{ "role": "user", "content": "Hello" }]
        }),
    );
    assert_eq!(resp.status, 200);
    let v: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "From Gemini");

    let sse = client_post(
        &format!("{}/v1/chat/completions", gem_handle.base_url()),
        &json!({
            "model": "gemini-2.5-flash",
            "stream": true,
            "messages": [{ "role": "user", "content": "Hello" }]
        }),
    );
    let sse_text = String::from_utf8(sse.body).unwrap();
    assert!(sse_text.contains("Gem chunk"));
    assert!(sse_text.contains("data: [DONE]"));
    gem_handle.stop();

    // 2. OpenAI -> Anthropic (JSON + SSE)
    let anth_upstream = Arc::new(MockUpstream::new(|_, url, headers, body| {
        assert_eq!(url, "https://api.anthropic.com/v1/messages");
        assert!(has_header(headers, "x-api-key", "upstream-secret-key-xyz"));
        assert!(has_header(headers, "anthropic-version", "2023-06-01"));
        let stream = body
            .unwrap()
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if stream {
            let sse = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Anth chunk\"}}\n\n";
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/event-stream".into())],
                body: sse.as_bytes().to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&json!({
                    "id": "msg_1",
                    "type": "message",
                    "model": "claude-sonnet-4-20250514",
                    "content": [{ "type": "text", "text": "From Anthropic" }],
                    "stop_reason": "end_turn",
                    "usage": { "input_tokens": 9, "output_tokens": 3 }
                }))
                .unwrap(),
            })
        }
    }));
    let mut anth_handle = start_loopback_bridge(
        base_target(
            EndpointProtocol::Anthropic,
            "https://api.anthropic.com",
            AuthScheme::XApiKey,
        ),
        anth_upstream,
    )
    .unwrap();
    let resp = client_post(
        &format!("{}/v1/chat/completions", anth_handle.base_url()),
        &json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                { "role": "system", "content": "Be concise" },
                { "role": "user", "content": "Hi" }
            ]
        }),
    );
    assert_eq!(resp.status, 200);
    let v: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "From Anthropic");

    let sse = client_post(
        &format!("{}/v1/chat/completions", anth_handle.base_url()),
        &json!({
            "model": "claude-sonnet-4-20250514",
            "stream": true,
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    let sse_text = String::from_utf8(sse.body).unwrap();
    assert!(sse_text.contains("Anth chunk"));
    assert!(sse_text.contains("data: [DONE]"));
    anth_handle.stop();

    // 3. OpenAI -> Ollama (JSON + NDJSON streaming)
    let ollama_upstream = Arc::new(MockUpstream::new(|_, url, _, body| {
        assert_eq!(url, "http://127.0.0.1:11434/api/chat");
        let stream = body
            .unwrap()
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if stream {
            let ndjson = concat!(
                "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"Ol\"},\"done\":false}\n",
                "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"lama!\"},\"done\":true}\n"
            );
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/x-ndjson".into())],
                body: ndjson.as_bytes().to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&json!({
                    "model": "llama3.2",
                    "message": { "role": "assistant", "content": "From Ollama" },
                    "done": true,
                    "done_reason": "stop",
                    "prompt_eval_count": 4,
                    "eval_count": 2
                }))
                .unwrap(),
            })
        }
    }));
    let mut ollama_handle = start_loopback_bridge(
        base_target(
            EndpointProtocol::Ollama,
            "http://127.0.0.1:11434",
            AuthScheme::None,
        ),
        ollama_upstream,
    )
    .unwrap();
    let resp = client_post(
        &format!("{}/v1/chat/completions", ollama_handle.base_url()),
        &json!({
            "model": "llama3.2",
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    assert_eq!(resp.status, 200);
    let v: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "From Ollama");

    let sse = client_post(
        &format!("{}/v1/chat/completions", ollama_handle.base_url()),
        &json!({
            "model": "llama3.2",
            "stream": true,
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    let sse_text = String::from_utf8(sse.body).unwrap();
    assert!(sse_text.contains("\"Ol\""));
    assert!(sse_text.contains("\"lama!\""));
    assert!(sse_text.contains("data: [DONE]"));
    ollama_handle.stop();
}

#[test]
fn bridge_serves_model_lists_and_redacts_upstream_errors() {
    let upstream = Arc::new(MockUpstream::new(|_, _, _, _| {
        Ok(HttpResponse {
            status: 401,
            headers: vec![],
            body: br#"{"error":{"message":"invalid token upstream-secret-key-xyz"}}"#.to_vec(),
        })
    }));
    let mut handle = start_loopback_bridge(
        base_target(
            EndpointProtocol::OpenAi,
            "https://api.openai.com/v1",
            AuthScheme::Bearer,
        ),
        upstream,
    )
    .unwrap();
    let base = handle.base_url();

    // Model list endpoints in OpenAI, Gemini, and Ollama shapes
    let openai_models: Value =
        serde_json::from_slice(&client_get(&format!("{base}/v1/models")).body).unwrap();
    assert_eq!(openai_models["data"][0]["id"], "test-model");

    let gem_models: Value =
        serde_json::from_slice(&client_get(&format!("{base}/v1beta/models")).body).unwrap();
    assert_eq!(gem_models["models"][0]["name"], "models/test-model");

    let ollama_models: Value =
        serde_json::from_slice(&client_get(&format!("{base}/api/tags")).body).unwrap();
    assert_eq!(ollama_models["models"][0]["name"], "test-model");

    // Upstream 401 error is sanitized (never leaks API key)
    let err_resp = client_post(
        &format!("{base}/v1/chat/completions"),
        &json!({
            "model": "test-model",
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    assert_eq!(err_resp.status, 401);
    let err_body = String::from_utf8(err_resp.body).unwrap();
    assert!(
        !err_body.contains("upstream-secret-key-xyz"),
        "leaked upstream key: {err_body}"
    );
    assert!(err_body.contains("<redacted>"));

    handle.stop();
}

#[test]
fn openai_client_to_responses_azure_and_cohere_upstreams() {
    // 1. OpenAI -> OpenAI Responses
    let resp_upstream = Arc::new(MockUpstream::new(|_, url, _, body| {
        assert_eq!(url, "https://api.openai.com/v1/responses");
        let b = body.unwrap();
        assert_eq!(b["instructions"], "Sys");
        Ok(HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&json!({
                "id": "resp_1",
                "model": "gpt-4.1",
                "output_text": "From Responses API",
                "usage": { "input_tokens": 5, "output_tokens": 3 }
            }))
            .unwrap(),
        })
    }));
    let mut h1 = start_loopback_bridge(
        base_target(
            EndpointProtocol::OpenAiResponses,
            "https://api.openai.com/v1",
            AuthScheme::Bearer,
        ),
        resp_upstream,
    )
    .unwrap();
    let r1 = client_post(
        &format!("{}/v1/chat/completions", h1.base_url()),
        &json!({
            "model": "gpt-4.1",
            "messages": [
                { "role": "system", "content": "Sys" },
                { "role": "user", "content": "Hi" }
            ]
        }),
    );
    let v1: Value = serde_json::from_slice(&r1.body).unwrap();
    assert_eq!(v1["choices"][0]["message"]["content"], "From Responses API");
    h1.stop();

    // 2. OpenAI -> Azure OpenAI
    let az_upstream = Arc::new(MockUpstream::new(|_, url, headers, _| {
        assert_eq!(
            url,
            "https://myres.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-10-21"
        );
        assert!(has_header(headers, "api-key", "upstream-secret-key-xyz"));
        Ok(HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&json!({
                "id": "az_1",
                "model": "gpt-4o",
                "choices": [{ "message": { "content": "From Azure" }, "finish_reason": "stop" }]
            }))
            .unwrap(),
        })
    }));
    let mut az_target = base_target(
        EndpointProtocol::AzureOpenAi,
        "https://myres.openai.azure.com",
        AuthScheme::ApiKey,
    );
    az_target.api_version = Some("2024-10-21".into());
    let mut h2 = start_loopback_bridge(az_target, az_upstream).unwrap();
    let r2 = client_post(
        &format!("{}/v1/chat/completions", h2.base_url()),
        &json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    let v2: Value = serde_json::from_slice(&r2.body).unwrap();
    assert_eq!(v2["choices"][0]["message"]["content"], "From Azure");
    h2.stop();

    // 3. OpenAI -> Cohere
    let co_upstream = Arc::new(MockUpstream::new(|_, url, headers, _| {
        assert_eq!(url, "https://api.cohere.com/v2/chat");
        assert!(has_header(
            headers,
            "authorization",
            "Bearer upstream-secret-key-xyz"
        ));
        Ok(HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&json!({
                "id": "co_1",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "From Cohere" }] },
                "finish_reason": "COMPLETE",
                "usage": { "billed_units": { "input_tokens": 6, "output_tokens": 2 } }
            }))
            .unwrap(),
        })
    }));
    let mut h3 = start_loopback_bridge(
        base_target(
            EndpointProtocol::Cohere,
            "https://api.cohere.com/v2",
            AuthScheme::Bearer,
        ),
        co_upstream,
    )
    .unwrap();
    let r3 = client_post(
        &format!("{}/v1/chat/completions", h3.base_url()),
        &json!({
            "model": "command-a-03-2025",
            "messages": [{ "role": "user", "content": "Hi" }]
        }),
    );
    let v3: Value = serde_json::from_slice(&r3.body).unwrap();
    assert_eq!(v3["choices"][0]["message"]["content"], "From Cohere");
    h3.stop();
}

fn raw_request(addr: std::net::SocketAddr, request: &str) -> String {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf
}

#[test]
fn local_token_rejects_missing_or_wrong_bearer_and_does_not_forward() {
    let upstream = Arc::new(MockUpstream::new(|_, _, _, _| {
        Ok(HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: br#"{"ok":true}"#.to_vec(),
        })
    }));
    let mut target = base_target(
        EndpointProtocol::OpenAi,
        "https://api.openai.com/v1",
        AuthScheme::Bearer,
    );
    target.local_token = Some(Zeroizing::new("bridge-local-token".into()));
    let mut handle = start_loopback_bridge(target, upstream.clone()).unwrap();
    let addr = handle.addr();

    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    let chat = |auth: Option<&str>| {
        let mut req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(auth) = auth {
            req.push_str(auth);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        req.push_str(body);
        req
    };

    let missing = raw_request(addr, &chat(None));
    assert!(missing.starts_with("HTTP/1.1 401"), "{missing}");
    assert!(!missing.contains("bridge-local-token"));
    assert!(
        upstream.take_calls().is_empty(),
        "missing token was forwarded"
    );

    let wrong = raw_request(addr, &chat(Some("Authorization: Bearer wrong-token")));
    assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
    assert!(!wrong.contains("bridge-local-token"));
    assert!(
        upstream.take_calls().is_empty(),
        "wrong token was forwarded"
    );

    let options = raw_request(
        addr,
        "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
    );
    assert!(options.starts_with("HTTP/1.1 204"), "{options}");
    assert!(upstream.take_calls().is_empty(), "OPTIONS was forwarded");

    let ok = raw_request(
        addr,
        &chat(Some("Authorization: Bearer bridge-local-token")),
    );
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert_eq!(upstream.take_calls().len(), 1);

    handle.stop();
}
