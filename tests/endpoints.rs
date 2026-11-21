use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use agyva::core::endpoints::{self, EndpointCache, EndpointPatch, EndpointRegistry, NewEndpoint};
use agyva::core::error::ErrorKind;
use agyva::core::protocol::{
    AuthScheme, EndpointProtocol, FetchedModel, HttpResponse, HttpTransport, TransportError,
    fetch_models, find_preset, join_endpoint_url, join_ollama_url, presets, test_endpoint,
    validate_endpoint_url,
};
use agyva::paths::{AgyPaths, Dirs};
use agyva::storage::vault::{Vault, VaultKey};
use serde_json::Value;
use zeroize::Zeroizing;

struct RecordedCall;

type Handler = Box<
    dyn Fn(&str, &str, &[(String, String)]) -> Result<HttpResponse, TransportError> + Send + Sync,
>;

struct MockTransport {
    calls: Mutex<Vec<RecordedCall>>,
    handler: Handler,
}

impl MockTransport {
    fn new(
        handler: impl Fn(&str, &str, &[(String, String)]) -> Result<HttpResponse, TransportError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            handler: Box::new(handler),
        }
    }

    fn calls(&self) -> Vec<RecordedCall> {
        let mut guard = self.calls.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

impl HttpTransport for MockTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        _body: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        self.calls.lock().unwrap().push(RecordedCall);
        (self.handler)(method, url, headers)
    }
}

fn has_header(headers: &[(String, String)], name: &str, expected: &str) -> bool {
    headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(name) && v == expected)
}

#[test]
fn protocol_enum_parsing_and_presets() {
    for (raw, expected) in [
        ("auto", EndpointProtocol::Auto),
        ("openai", EndpointProtocol::OpenAi),
        ("openai-responses", EndpointProtocol::OpenAiResponses),
        ("anthropic", EndpointProtocol::Anthropic),
        ("gemini", EndpointProtocol::Gemini),
        ("azure-openai", EndpointProtocol::AzureOpenAi),
        ("ollama", EndpointProtocol::Ollama),
        ("cohere", EndpointProtocol::Cohere),
        ("custom", EndpointProtocol::Custom),
    ] {
        let parsed: EndpointProtocol = raw.parse().unwrap();
        assert_eq!(parsed, expected);
        assert_eq!(parsed.to_string(), raw);
        let json = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, format!("\"{raw}\""));
        let roundtrip: EndpointProtocol = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, expected);
    }

    assert_eq!(presets().len(), 17);
    assert_eq!(
        find_preset("openrouter").unwrap().base_url,
        "https://openrouter.ai/api/v1"
    );
    assert_eq!(
        find_preset("azure").unwrap().protocol,
        EndpointProtocol::AzureOpenAi
    );
}

#[test]
fn url_normalization_and_validation_rules() {
    assert_eq!(
        join_endpoint_url("https://api.openai.com/v1", "/v1/models"),
        "https://api.openai.com/v1/models"
    );
    assert_eq!(
        join_endpoint_url("https://api.openai.com/v1/", "/v1/models"),
        "https://api.openai.com/v1/models"
    );
    assert_eq!(
        join_endpoint_url("https://api.openai.com", "/v1/models"),
        "https://api.openai.com/v1/models"
    );
    assert_eq!(
        join_endpoint_url(
            "https://generativelanguage.googleapis.com/v1beta/",
            "/v1beta/models"
        ),
        "https://generativelanguage.googleapis.com/v1beta/models"
    );
    assert_eq!(
        join_ollama_url("http://127.0.0.1:11434/v1", "/api/tags"),
        "http://127.0.0.1:11434/api/tags"
    );

    assert!(validate_endpoint_url("https://api.openai.com/v1").is_ok());
    assert!(validate_endpoint_url("http://127.0.0.1:11434").is_ok());
    assert!(validate_endpoint_url("http://localhost:8000/v1").is_ok());
    assert!(validate_endpoint_url("http://example.com/v1").is_err());
    assert!(validate_endpoint_url("http://169.254.169.254/latest").is_err());
}

#[test]
fn fetch_models_all_protocols_and_rate_limits() {
    // 1. OpenAI + OpenAiResponses + Custom + rate limit headers
    let openai_tr = MockTransport::new(|_, url, headers| {
        assert_eq!(url, "https://api.openai.com/v1/models");
        assert!(has_header(
            headers,
            "authorization",
            "Bearer sk-openai-test"
        ));
        Ok(HttpResponse {
            status: 200,
            headers: vec![
                ("x-ratelimit-limit-requests".into(), "500".into()),
                ("x-ratelimit-remaining-requests".into(), "499".into()),
                ("x-ratelimit-limit-tokens".into(), "100000".into()),
                ("x-ratelimit-remaining-tokens".into(), "99500".into()),
                ("x-ratelimit-reset-requests".into(), "1s".into()),
            ],
            body: br#"{"data":[{"id":"gpt-4o","owned_by":"openai","context_length":128000}]}"#
                .to_vec(),
        })
    });
    for proto in [
        EndpointProtocol::OpenAi,
        EndpointProtocol::OpenAiResponses,
        EndpointProtocol::Custom,
    ] {
        let res = fetch_models(
            &openai_tr,
            "https://api.openai.com/v1/",
            proto,
            None,
            "sk-openai-test",
            None,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(res.detected_protocol, proto);
        assert_eq!(res.models.len(), 1);
        assert_eq!(res.models[0].id, "gpt-4o");
        assert_eq!(res.models[0].context_window, Some(128000));
        let rl = res.rate_limits.unwrap();
        assert_eq!(rl.requests_limit, Some(500));
        assert_eq!(rl.requests_remaining, Some(499));
        assert_eq!(rl.tokens_limit, Some(100000));
        assert_eq!(rl.tokens_remaining, Some(99500));
        assert_eq!(rl.reset_hint.as_deref(), Some("1s"));
    }

    // 2. Anthropic
    let anth_tr = MockTransport::new(|_, url, headers| {
        assert_eq!(url, "https://api.anthropic.com/v1/models");
        assert!(has_header(headers, "x-api-key", "sk-ant-test"));
        assert!(has_header(headers, "anthropic-version", "2023-06-01"));
        Ok(HttpResponse {
            status: 200,
            headers: vec![
                ("anthropic-ratelimit-requests-limit".into(), "100".into()),
                ("anthropic-ratelimit-requests-remaining".into(), "97".into()),
                (
                    "anthropic-ratelimit-requests-reset".into(),
                    "2026-01-01T00:00:00Z".into(),
                ),
            ],
            body:
                br#"{"data":[{"id":"claude-sonnet-4-20250514","display_name":"Claude Sonnet 4"}]}"#
                    .to_vec(),
        })
    });
    let res = fetch_models(
        &anth_tr,
        "https://api.anthropic.com",
        EndpointProtocol::Anthropic,
        None,
        "sk-ant-test",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.models[0].id, "claude-sonnet-4-20250514");
    assert_eq!(
        res.models[0].display_name.as_deref(),
        Some("Claude Sonnet 4")
    );
    assert_eq!(res.rate_limits.unwrap().requests_remaining, Some(97));

    // 3. Gemini (strips models/ prefix)
    let gem_tr = MockTransport::new(|_, url, headers| {
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        assert!(has_header(headers, "x-goog-api-key", "gem-key"));
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: br#"{"models":[{"name":"models/gemini-2.5-flash","displayName":"Gemini 2.5 Flash","inputTokenLimit":1048576,"outputTokenLimit":65536,"supportedGenerationMethods":["generateContent"]}]}"#.to_vec(),
        })
    });
    let res = fetch_models(
        &gem_tr,
        "https://generativelanguage.googleapis.com/v1beta",
        EndpointProtocol::Gemini,
        None,
        "gem-key",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.models[0].id, "gemini-2.5-flash");
    assert_eq!(res.models[0].context_window, Some(1_048_576));
    assert_eq!(res.models[0].max_output_tokens, Some(65536));
    assert_eq!(res.models[0].capabilities, vec!["generateContent"]);

    // 4. Azure OpenAI
    let az_tr = MockTransport::new(|_, url, headers| {
        assert_eq!(
            url,
            "https://myres.openai.azure.com/openai/models?api-version=2024-10-21"
        );
        assert!(has_header(headers, "api-key", "az-secret"));
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"gpt-4o","capabilities":{"chat_completion":true,"embeddings":false}}]}"#.to_vec(),
        })
    });
    let res = fetch_models(
        &az_tr,
        "https://myres.openai.azure.com",
        EndpointProtocol::AzureOpenAi,
        None,
        "az-secret",
        Some("2024-10-21"),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.models[0].id, "gpt-4o");
    assert_eq!(res.models[0].capabilities, vec!["chat_completion"]);

    // 5. Ollama
    let ollama_tr = MockTransport::new(|_, url, _| {
        assert_eq!(url, "http://127.0.0.1:11434/api/tags");
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: br#"{"models":[{"name":"llama3.2:latest","details":{"family":"llama"}}]}"#
                .to_vec(),
        })
    });
    let res = fetch_models(
        &ollama_tr,
        "http://127.0.0.1:11434",
        EndpointProtocol::Ollama,
        None,
        "",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.models[0].id, "llama3.2:latest");
    assert_eq!(res.models[0].owned_by.as_deref(), Some("llama"));

    // 6. Cohere
    let cohere_tr = MockTransport::new(|_, url, headers| {
        assert_eq!(url, "https://api.cohere.com/v2/models");
        assert!(has_header(headers, "authorization", "Bearer co-key"));
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: br#"{"models":[{"name":"command-a-03-2025","context_length":256000,"endpoints":["chat"]}]}"#.to_vec(),
        })
    });
    let res = fetch_models(
        &cohere_tr,
        "https://api.cohere.com/v2",
        EndpointProtocol::Cohere,
        None,
        "co-key",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.models[0].id, "command-a-03-2025");
    assert_eq!(res.models[0].context_window, Some(256000));
    assert_eq!(res.models[0].capabilities, vec!["chat"]);

    // 7. Bare array + OpenRouter credit check
    let or_tr = MockTransport::new(|_, url, _| {
        if url == "https://openrouter.ai/api/v1/models" {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"data":[{"id":"openai/gpt-4o","name":"OpenAI: GPT-4o","context_length":128000}]}"#.to_vec(),
            })
        } else if url == "https://openrouter.ai/api/v1/auth/key" {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"data":{"limit":25.0,"limit_remaining":18.5}}"#.to_vec(),
            })
        } else {
            panic!("unexpected url {url}");
        }
    });
    let res = fetch_models(
        &or_tr,
        "https://openrouter.ai/api/v1",
        EndpointProtocol::OpenAi,
        None,
        "or-key",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.models[0].id, "openai/gpt-4o");
    assert_eq!(
        res.models[0].display_name.as_deref(),
        Some("OpenAI: GPT-4o")
    );
    let rl = res.rate_limits.unwrap();
    assert_eq!(rl.credits_limit, Some(25.0));
    assert_eq!(rl.credits_remaining, Some(18.5));

    // Bare array fallback
    let bare_tr = MockTransport::new(|_, _, _| {
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: br#"["alpha-1","beta-2"]"#.to_vec(),
        })
    });
    let res = fetch_models(
        &bare_tr,
        "https://proxy.example.com",
        EndpointProtocol::Custom,
        None,
        "k",
        None,
        Some("/models"),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.models.len(), 2);
    assert_eq!(res.models[0].id, "alpha-1");
}

#[test]
fn auto_protocol_detection_probes_neutral_hosts() {
    // Neutral URL serving only Anthropic
    let anth_mock = MockTransport::new(|_, url, headers| {
        if url.ends_with("/v1/models")
            && has_header(headers, "x-api-key", "secret-123")
            && has_header(headers, "anthropic-version", "2023-06-01")
        {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"data":[{"id":"claude-3-7-sonnet"}]}"#.to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 401,
                headers: vec![],
                body: br#"{"error":"missing x-api-key"}"#.to_vec(),
            })
        }
    });
    let res = fetch_models(
        &anth_mock,
        "https://neutral.example.com",
        EndpointProtocol::Auto,
        None,
        "secret-123",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.detected_protocol, EndpointProtocol::Anthropic);
    assert_eq!(res.models[0].id, "claude-3-7-sonnet");
    assert!(anth_mock.calls().len() >= 2);

    // Neutral URL serving only Gemini
    let gem_mock = MockTransport::new(|_, url, headers| {
        if url.ends_with("/v1beta/models") && has_header(headers, "x-goog-api-key", "g-secret") {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"models":[{"name":"models/gemini-2.5-pro"}]}"#.to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 404,
                headers: vec![],
                body: b"not found".to_vec(),
            })
        }
    });
    let res = fetch_models(
        &gem_mock,
        "https://neutral.example.com",
        EndpointProtocol::Auto,
        None,
        "g-secret",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.detected_protocol, EndpointProtocol::Gemini);
    assert_eq!(res.models[0].id, "gemini-2.5-pro");

    // Neutral URL serving only Ollama
    let ollama_mock = MockTransport::new(|_, url, _| {
        if url.ends_with("/api/tags") {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"models":[{"name":"qwen2.5-coder:7b"}]}"#.to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 404,
                headers: vec![],
                body: b"not found".to_vec(),
            })
        }
    });
    let res = fetch_models(
        &ollama_mock,
        "https://neutral.example.com",
        EndpointProtocol::Auto,
        None,
        "",
        None,
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(res.detected_protocol, EndpointProtocol::Ollama);
    assert_eq!(res.models[0].id, "qwen2.5-coder:7b");

    // Probe error never leaks API key
    let err_mock = MockTransport::new(|_, _, _| {
        Ok(HttpResponse {
            status: 401,
            headers: vec![],
            body: br#"{"error":{"message":"bad key sk-top-secret-999"}}"#.to_vec(),
        })
    });
    let probe = test_endpoint(
        &err_mock,
        "https://api.openai.com/v1",
        EndpointProtocol::OpenAi,
        None,
        "sk-top-secret-999",
        None,
        None,
        &BTreeMap::new(),
    );
    assert!(!probe.reachable);
    let err = probe.error.unwrap();
    assert!(!err.contains("sk-top-secret-999"), "leaked key in: {err}");
    assert!(err.contains("<redacted>"));
}

#[test]
fn encrypted_registry_crud_redaction_and_env_export() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(dir.path());
    let vault = Vault::new(VaultKey::generate().unwrap());

    let mut extra = BTreeMap::new();
    extra.insert("X-Title".into(), "agyva-test".into());
    let mut secret_headers = BTreeMap::new();
    secret_headers.insert("X-Secret-Token".into(), "hdr-secret-xyz".into());

    let new_ep = NewEndpoint {
        alias: "work-or".into(),
        base_url: "https://openrouter.ai/api/v1/".into(),
        protocol: EndpointProtocol::Auto,
        auth_scheme: Some(AuthScheme::Bearer),
        api_key: Zeroizing::new("sk-or-super-secret-value".into()),
        default_model: Some("openai/gpt-4o".into()),
        api_version: None,
        models_path: None,
        chat_path: None,
        extra_headers: extra,
        secret_headers,
        enabled: true,
    };

    // Debug must redact secrets
    let dbg = format!("{new_ep:?}");
    assert!(!dbg.contains("sk-or-super-secret-value"));
    assert!(!dbg.contains("hdr-secret-xyz"));
    assert!(dbg.contains("<redacted>"));

    let rec = endpoints::add_endpoint(&dirs, &vault, new_ep).unwrap();
    assert_eq!(rec.alias, "work-or");
    assert_eq!(rec.base_url, "https://openrouter.ai/api/v1");

    // Secret snapshot exists and is encrypted; endpoints.json has NO plaintext secret
    let snap_path = endpoints::secret_file(&dirs, &rec.id);
    assert!(snap_path.exists());
    let snap_raw = std::fs::read(&snap_path).unwrap();
    assert!(!snap_raw.windows(10).any(|w| w == b"sk-or-supe"));

    let reg_text = std::fs::read_to_string(endpoints::registry_file(&dirs)).unwrap();
    assert!(!reg_text.contains("sk-or-super-secret-value"));
    assert!(!reg_text.contains("hdr-secret-xyz"));

    // Reload secret via vault
    let loaded_secret = endpoints::load_secret(&dirs, &vault, &rec.id).unwrap();
    assert_eq!(loaded_secret.api_key.as_str(), "sk-or-super-secret-value");
    assert_eq!(
        loaded_secret
            .secret_headers
            .get("X-Secret-Token")
            .map(String::as_str),
        Some("hdr-secret-xyz")
    );
    let sec_dbg = format!("{loaded_secret:?}");
    assert!(!sec_dbg.contains("sk-or-super-secret-value"));
    assert!(!sec_dbg.contains("hdr-secret-xyz"));

    // Wrong vault key fails decryption
    let wrong_vault = Vault::new(VaultKey::generate().unwrap());
    assert!(endpoints::load_secret(&dirs, &wrong_vault, &rec.id).is_err());

    // Refresh models updates cache & detected_protocol without leaking secrets
    let mock = MockTransport::new(|_, url, headers| {
        if url.ends_with("/models") {
            assert!(has_header(headers, "x-secret-token", "hdr-secret-xyz"));
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"data":[{"id":"openai/gpt-4o","context_length":128000}]}"#.to_vec(),
            })
        } else {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"data":{"limit":10.0,"limit_remaining":9.5}}"#.to_vec(),
            })
        }
    });
    let conn_probe = endpoints::test_endpoint_connection(&dirs, &vault, &mock, "work-or").unwrap();
    assert!(conn_probe.reachable);
    assert_eq!(conn_probe.model_count, 1);

    let probe = endpoints::refresh_endpoint_models(&dirs, &vault, &mock, "work-or").unwrap();
    assert!(probe.reachable);
    assert_eq!(probe.detected_protocol, Some(EndpointProtocol::OpenAi));

    let cache_text = std::fs::read_to_string(endpoints::cache_file(&dirs)).unwrap();
    assert!(!cache_text.contains("sk-or-super-secret-value"));
    assert!(!cache_text.contains("hdr-secret-xyz"));
    let cache = EndpointCache::new(&dirs).load();
    assert_eq!(cache.endpoints[&rec.id].models[0].id, "openai/gpt-4o");

    // Selectors: alias, 1-based index, id, id prefix
    let reg = EndpointRegistry::load(&endpoints::registry_file(&dirs)).unwrap();
    assert_eq!(
        endpoints::resolve_endpoint(&reg, "WORK-OR").unwrap().id,
        rec.id
    );
    assert_eq!(endpoints::resolve_endpoint(&reg, "1").unwrap().id, rec.id);
    assert_eq!(
        endpoints::resolve_endpoint(&reg, &rec.id).unwrap().id,
        rec.id
    );
    assert_eq!(
        endpoints::resolve_endpoint(&reg, &rec.id[..4]).unwrap().id,
        rec.id
    );
    assert_eq!(
        endpoints::resolve_endpoint(&reg, "missing")
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );

    // Active endpoint + rename + update + env export
    let active = endpoints::set_active_endpoint(&dirs, "work-or").unwrap();
    assert!(active.last_used_at.is_some());

    let renamed = endpoints::rename_endpoint(&dirs, "work-or", "openrouter").unwrap();
    assert_eq!(renamed.alias, "openrouter");

    let mut patch = EndpointPatch::default();
    patch.default_model = Some(Some("anthropic/claude-3.7-sonnet".into()));
    let updated = endpoints::update_endpoint(&dirs, &vault, "openrouter", patch).unwrap();
    assert_eq!(
        updated.default_model.as_deref(),
        Some("anthropic/claude-3.7-sonnet")
    );

    let env_map: HashMap<String, String> = endpoints::export_env_vars(
        &updated,
        &loaded_secret,
        None,
        Some("http://127.0.0.1:43210"),
    )
    .into_iter()
    .collect();
    assert_eq!(
        env_map.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-or-super-secret-value")
    );
    assert_eq!(
        env_map.get("OPENAI_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:43210")
    );
    assert_eq!(
        env_map.get("GOOGLE_GEMINI_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:43210")
    );
    assert_eq!(
        env_map.get("AGYVA_MODEL").map(String::as_str),
        Some("anthropic/claude-3.7-sonnet")
    );

    endpoints::clear_active_endpoint(&dirs).unwrap();
    let reg = endpoints::load_registry(&dirs).unwrap();
    assert!(reg.active_endpoint.is_none());

    endpoints::remove_endpoint(&dirs, "openrouter").unwrap();
    assert!(!snap_path.exists());
    assert!(endpoints::list_endpoints(&dirs).unwrap().is_empty());
}

#[test]
fn agy_settings_sync_and_clear_preserve_existing_keys_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let agy = AgyPaths {
        home: dir.path().join("agy"),
    };
    std::fs::create_dir_all(&agy.home).unwrap();
    std::fs::write(
        agy.settings(),
        r#"{"theme":"dark","n":1.50,"gcp":{"project":"my-proj"},"arr":[1,2]}"#,
    )
    .unwrap();

    let dirs = Dirs::under(dir.path());
    let vault = Vault::new(VaultKey::generate().unwrap());
    let mut new_vllm = NewEndpoint::default();
    new_vllm.alias = "local-vllm".into();
    new_vllm.base_url = "http://127.0.0.1:8000/v1".into();
    new_vllm.protocol = EndpointProtocol::OpenAi;
    new_vllm.api_key = Zeroizing::new("super-secret-should-not-appear".into());
    new_vllm.default_model = Some("qwen3-32b".into());
    let rec = endpoints::add_endpoint(&dirs, &vault, new_vllm).unwrap();

    let models = vec![FetchedModel {
        id: "qwen3-32b".into(),
        display_name: Some("Qwen 3 32B".into()),
        context_window: Some(32768),
        max_output_tokens: Some(8192),
        owned_by: None,
        capabilities: vec![],
    }];

    endpoints::sync_agy_settings(&agy, &rec, &models, Some("http://127.0.0.1:45555")).unwrap();

    let text = std::fs::read_to_string(agy.settings()).unwrap();
    assert!(!text.contains("super-secret-should-not-appear"));
    assert!(text.contains("1.50"), "number formatting preserved: {text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["gcp"]["project"], "my-proj");
    assert_eq!(v["arr"], serde_json::json!([1, 2]));
    assert_eq!(v["modelProvider"], "custom");
    assert_eq!(v["customModelsConfig"]["baseUrl"], "http://127.0.0.1:45555");
    assert_eq!(v["customModelsConfig"]["models"][0]["id"], "qwen3-32b");

    endpoints::clear_agy_custom_provider(&agy).unwrap();
    let cleared_text = std::fs::read_to_string(agy.settings()).unwrap();
    let v2: Value = serde_json::from_str(&cleared_text).unwrap();
    assert!(v2.get("modelProvider").is_none());
    assert!(v2.get("customModelsConfig").is_none());
    assert_eq!(v2["theme"], "dark");
    assert_eq!(v2["gcp"]["project"], "my-proj");

    // Corrupt settings.json must fail closed without touching the file
    std::fs::write(agy.settings(), "{broken json").unwrap();
    assert!(endpoints::sync_agy_settings(&agy, &rec, &models, None).is_err());
    assert!(endpoints::clear_agy_custom_provider(&agy).is_err());
    assert_eq!(
        std::fs::read_to_string(agy.settings()).unwrap(),
        "{broken json"
    );
}
