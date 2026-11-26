//! Endpoint protocols, provider presets, HTTP transport abstraction,
//! model discovery, and rate-limit probing.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Cursor, Read};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::core::error::{Error, Result};
use crate::core::format::sanitize;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_AZURE_API_VERSION: &str = "2024-10-21";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Hops we will follow ourselves. ureq must not follow: it strips only
/// `Authorization` and would send API-key and secret headers to the next host.
const MAX_REDIRECTS: u8 = 3;

/// Supported LLM endpoint wire protocols.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointProtocol {
    #[default]
    Auto,
    #[serde(rename = "openai", alias = "open-ai")]
    OpenAi,
    #[serde(rename = "openai-responses", alias = "open-ai-responses")]
    OpenAiResponses,
    Anthropic,
    Gemini,
    #[serde(rename = "azure-openai", alias = "azure-open-ai")]
    AzureOpenAi,
    Ollama,
    Cohere,
    Custom,
}

impl EndpointProtocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OpenAi => "openai",
            Self::OpenAiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::AzureOpenAi => "azure-openai",
            Self::Ollama => "ollama",
            Self::Cohere => "cohere",
            Self::Custom => "custom",
        }
    }

    pub fn default_auth_scheme(self) -> AuthScheme {
        match self {
            Self::Anthropic => AuthScheme::XApiKey,
            Self::Gemini => AuthScheme::XGoogApiKey,
            Self::AzureOpenAi => AuthScheme::ApiKey,
            Self::Ollama => AuthScheme::None,
            Self::Auto | Self::OpenAi | Self::OpenAiResponses | Self::Cohere | Self::Custom => {
                AuthScheme::Bearer
            }
        }
    }
}

impl fmt::Display for EndpointProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EndpointProtocol {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "openai" | "open-ai" | "openai-chat" => Ok(Self::OpenAi),
            "openai-responses" | "open-ai-responses" | "responses" => Ok(Self::OpenAiResponses),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "gemini" | "google" | "google-gemini" => Ok(Self::Gemini),
            "azure-openai" | "azure-open-ai" | "azure" => Ok(Self::AzureOpenAi),
            "ollama" => Ok(Self::Ollama),
            "cohere" => Ok(Self::Cohere),
            "custom" => Ok(Self::Custom),
            other => Err(format!(
                "unknown protocol `{other}` (expected auto, openai, openai-responses, anthropic, gemini, azure-openai, ollama, cohere, or custom)"
            )),
        }
    }
}

/// How an API key is sent to an upstream endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthScheme {
    #[default]
    Bearer,
    XApiKey,
    XGoogApiKey,
    ApiKey,
    CustomHeader {
        name: String,
        #[serde(default)]
        prefix: String,
    },
    None,
}

impl AuthScheme {
    /// Append the authentication header to `headers` if an API key is present.
    pub fn apply(&self, headers: &mut Vec<(String, String)>, api_key: &str) {
        if api_key.is_empty() && !matches!(self, Self::None) {
            return;
        }
        match self {
            Self::Bearer => {
                headers.push(("Authorization".into(), format!("Bearer {api_key}")));
            }
            Self::XApiKey => {
                headers.push(("x-api-key".into(), api_key.to_owned()));
            }
            Self::XGoogApiKey => {
                headers.push(("x-goog-api-key".into(), api_key.to_owned()));
            }
            Self::ApiKey => {
                headers.push(("api-key".into(), api_key.to_owned()));
            }
            Self::CustomHeader { name, prefix } => {
                if name.trim().is_empty() {
                    return;
                }
                let val = if prefix.is_empty() {
                    api_key.to_owned()
                } else if prefix.ends_with(' ') {
                    format!("{prefix}{api_key}")
                } else {
                    format!("{prefix} {api_key}")
                };
                headers.push((name.trim().to_owned(), val));
            }
            Self::None => {}
        }
    }
}

impl fmt::Display for AuthScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer => f.write_str("bearer"),
            Self::XApiKey => f.write_str("x-api-key"),
            Self::XGoogApiKey => f.write_str("x-goog-api-key"),
            Self::ApiKey => f.write_str("api-key"),
            Self::CustomHeader { name, prefix } => {
                if prefix.is_empty() {
                    write!(f, "custom-header:{name}")
                } else {
                    write!(f, "custom-header:{name}:{prefix}")
                }
            }
            Self::None => f.write_str("none"),
        }
    }
}

impl FromStr for AuthScheme {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let trimmed = s.trim();
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "bearer" => Ok(Self::Bearer),
            "x-api-key" => Ok(Self::XApiKey),
            "x-goog-api-key" => Ok(Self::XGoogApiKey),
            "api-key" => Ok(Self::ApiKey),
            "none" => Ok(Self::None),
            _ => {
                if let Some(rest) = trimmed
                    .strip_prefix("custom-header:")
                    .or_else(|| trimmed.strip_prefix("custom:"))
                {
                    let (name, prefix) = match rest.split_once(':') {
                        Some((n, p)) => (n.trim(), p.trim()),
                        None => (rest.trim(), ""),
                    };
                    if name.is_empty() {
                        return Err("custom auth header name cannot be empty".into());
                    }
                    Ok(Self::CustomHeader {
                        name: name.to_owned(),
                        prefix: prefix.to_owned(),
                    })
                } else {
                    Err(format!(
                        "unknown auth scheme `{trimmed}` (expected bearer, x-api-key, x-goog-api-key, api-key, none, or custom-header:<name>[:<prefix>])"
                    ))
                }
            }
        }
    }
}

/// Known provider template for quick endpoint setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPreset {
    pub name: &'static str,
    pub display_name: &'static str,
    pub base_url: &'static str,
    pub protocol: EndpointProtocol,
    pub auth_scheme: AuthScheme,
    pub default_model: Option<&'static str>,
    pub api_version: Option<&'static str>,
}

static PRESETS: LazyLock<Vec<ProviderPreset>> = LazyLock::new(|| {
    vec![
        ProviderPreset {
            name: "openai",
            display_name: "OpenAI",
            base_url: "https://api.openai.com/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("gpt-4o"),
            api_version: None,
        },
        ProviderPreset {
            name: "anthropic",
            display_name: "Anthropic",
            base_url: "https://api.anthropic.com",
            protocol: EndpointProtocol::Anthropic,
            auth_scheme: AuthScheme::XApiKey,
            default_model: Some("claude-sonnet-4-20250514"),
            api_version: None,
        },
        ProviderPreset {
            name: "gemini",
            display_name: "Google Gemini",
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            protocol: EndpointProtocol::Gemini,
            auth_scheme: AuthScheme::XGoogApiKey,
            default_model: Some("gemini-2.5-flash"),
            api_version: None,
        },
        ProviderPreset {
            name: "openrouter",
            display_name: "OpenRouter",
            base_url: "https://openrouter.ai/api/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("openai/gpt-4o"),
            api_version: None,
        },
        ProviderPreset {
            name: "groq",
            display_name: "Groq",
            base_url: "https://api.groq.com/openai/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("llama-3.3-70b-versatile"),
            api_version: None,
        },
        ProviderPreset {
            name: "deepseek",
            display_name: "DeepSeek",
            base_url: "https://api.deepseek.com/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("deepseek-chat"),
            api_version: None,
        },
        ProviderPreset {
            name: "mistral",
            display_name: "Mistral AI",
            base_url: "https://api.mistral.ai/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("mistral-large-latest"),
            api_version: None,
        },
        ProviderPreset {
            name: "together",
            display_name: "Together AI",
            base_url: "https://api.together.xyz/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: None,
            api_version: None,
        },
        ProviderPreset {
            name: "fireworks",
            display_name: "Fireworks AI",
            base_url: "https://api.fireworks.ai/inference/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: None,
            api_version: None,
        },
        ProviderPreset {
            name: "xai",
            display_name: "xAI",
            base_url: "https://api.x.ai/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("grok-3"),
            api_version: None,
        },
        ProviderPreset {
            name: "perplexity",
            display_name: "Perplexity",
            base_url: "https://api.perplexity.ai",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("sonar"),
            api_version: None,
        },
        ProviderPreset {
            name: "ollama",
            display_name: "Ollama",
            base_url: "http://127.0.0.1:11434",
            protocol: EndpointProtocol::Ollama,
            auth_scheme: AuthScheme::None,
            default_model: None,
            api_version: None,
        },
        ProviderPreset {
            name: "lmstudio",
            display_name: "LM Studio",
            base_url: "http://127.0.0.1:1234/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::None,
            default_model: None,
            api_version: None,
        },
        ProviderPreset {
            name: "vllm",
            display_name: "vLLM",
            base_url: "http://127.0.0.1:8000/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::None,
            default_model: None,
            api_version: None,
        },
        ProviderPreset {
            name: "litellm",
            display_name: "LiteLLM",
            base_url: "http://127.0.0.1:4000/v1",
            protocol: EndpointProtocol::OpenAi,
            auth_scheme: AuthScheme::Bearer,
            default_model: None,
            api_version: None,
        },
        ProviderPreset {
            name: "azure",
            display_name: "Azure OpenAI",
            base_url: "https://YOUR_RESOURCE.openai.azure.com",
            protocol: EndpointProtocol::AzureOpenAi,
            auth_scheme: AuthScheme::ApiKey,
            default_model: None,
            api_version: Some(DEFAULT_AZURE_API_VERSION),
        },
        ProviderPreset {
            name: "cohere",
            display_name: "Cohere",
            base_url: "https://api.cohere.com/v2",
            protocol: EndpointProtocol::Cohere,
            auth_scheme: AuthScheme::Bearer,
            default_model: Some("command-a-03-2025"),
            api_version: None,
        },
    ]
});

/// All built-in provider presets.
pub fn presets() -> &'static [ProviderPreset] {
    &PRESETS
}

/// Find a provider preset by name (case-insensitive).
pub fn find_preset(name: &str) -> Option<&'static ProviderPreset> {
    let needle = name.trim().to_ascii_lowercase();
    PRESETS
        .iter()
        .find(|p| p.name == needle || p.display_name.eq_ignore_ascii_case(&needle))
}

/// A model discovered from an endpoint's model-listing API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// Rate-limit and credit metadata extracted from HTTP headers or key endpoints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_limit: Option<f64>,
}

impl RateLimitInfo {
    pub fn is_empty(&self) -> bool {
        self.requests_remaining.is_none()
            && self.requests_limit.is_none()
            && self.tokens_remaining.is_none()
            && self.tokens_limit.is_none()
            && self.reset_hint.is_none()
            && self.credits_remaining.is_none()
            && self.credits_limit.is_none()
    }
}

/// Result of fetching models from an endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchModelsResult {
    pub detected_protocol: EndpointProtocol,
    pub models: Vec<FetchedModel>,
    pub rate_limits: Option<RateLimitInfo>,
    pub latency_ms: u64,
}

/// Result of probing an endpoint's connectivity and models.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub reachable: bool,
    pub detected_protocol: Option<EndpointProtocol>,
    pub latency_ms: Option<u64>,
    pub model_count: usize,
    pub models: Vec<FetchedModel>,
    pub rate_limits: Option<RateLimitInfo>,
    pub error: Option<String>,
}

/// Buffered HTTP response returned by [`HttpTransport`].
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Streaming HTTP response returned by [`HttpTransport::request_stream`].
pub struct HttpStreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl fmt::Debug for HttpStreamResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpStreamResponse")
            .field("status", &self.status)
            .field("header_count", &self.headers.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("refused url: {0}")]
    Refused(String),
    #[error("request timed out: {0}")]
    Timeout(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("response too large (exceeded {0} bytes)")]
    TooLarge(usize),
}

/// Transport seam for endpoint HTTP calls; production uses [`UreqTransport`],
/// tests inject mocks.
pub trait HttpTransport: Send + Sync {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> std::result::Result<HttpResponse, TransportError>;

    fn request_stream(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> std::result::Result<HttpStreamResponse, TransportError> {
        let resp = self.request(method, url, headers, body, timeout)?;
        Ok(HttpStreamResponse {
            status: resp.status,
            headers: resp.headers,
            body: Box::new(Cursor::new(resp.body)),
        })
    }
}

/// Production HTTP transport backed by `ureq` (rustls).
#[derive(Debug, Default, Clone)]
pub struct UreqTransport;

impl UreqTransport {
    pub fn new() -> Self {
        Self
    }

    fn run_ureq(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> std::result::Result<ureq::http::Response<ureq::Body>, TransportError> {
        let mut method = method.to_owned();
        let mut url = url.to_owned();
        let mut headers = headers.to_vec();
        let mut body = body.map(|b| b.to_vec());
        for hop in 0..=MAX_REDIRECTS {
            let response = self.send_once(&method, &url, &headers, body.as_deref(), timeout)?;
            let status = response.status().as_u16();
            if !is_redirect(status) {
                return Ok(response);
            }
            if hop == MAX_REDIRECTS {
                return Err(TransportError::Refused(format!(
                    "refusing to follow more than {MAX_REDIRECTS} redirects from {url}"
                )));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    TransportError::Refused(format!("redirect from {url} has no Location"))
                })?
                .to_owned();
            drop(response);
            let next = resolve_redirect(&url, &location).map_err(TransportError::Refused)?;
            reject_redirect(&url, &next).map_err(TransportError::Refused)?;
            headers = headers_after_redirect(&headers, same_host(&url, &next));
            if redirect_drops_body(status, &method) {
                method = "GET".into();
                body = None;
                headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
            }
            url = next;
        }
        Err(TransportError::Refused(
            "refusing to follow another redirect".into(),
        ))
    }

    fn send_once(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> std::result::Result<ureq::http::Response<ureq::Body>, TransportError> {
        let is_local = validate_endpoint_url(url).map_err(TransportError::Refused)?;
        let http_method = ureq::http::Method::from_bytes(method.as_bytes())
            .map_err(|e| TransportError::Refused(format!("invalid HTTP method: {e}")))?;
        // max_redirects(0) is the control. A later history check cannot
        // unsend headers ureq already forwarded.
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .https_only(!is_local)
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        let agent: ureq::Agent = config.into();

        let mut builder = ureq::http::Request::builder()
            .method(http_method)
            .uri(url)
            .header("User-Agent", concat!("agyva/", env!("CARGO_PKG_VERSION")));
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }

        let response = match body {
            Some(bytes) => {
                let req = builder
                    .body(bytes)
                    .map_err(|e| TransportError::Network(format!("building request: {e}")))?;
                agent.run(req)
            }
            None => {
                let req = builder
                    .body(())
                    .map_err(|e| TransportError::Network(format!("building request: {e}")))?;
                agent.run(req)
            }
        }
        .map_err(|e| match e {
            ureq::Error::Timeout(_) => TransportError::Timeout(e.to_string()),
            other => TransportError::Network(other.to_string()),
        })?;
        Ok(response)
    }
}

impl HttpTransport for UreqTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> std::result::Result<HttpResponse, TransportError> {
        let response = self.run_ureq(method, url, headers, body, timeout)?;
        let status = response.status().as_u16();
        let resp_headers = collect_headers(response.headers());
        let mut buf = Vec::new();
        response
            .into_body()
            .into_reader()
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| TransportError::Network(format!("reading response body: {e}")))?;
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(TransportError::TooLarge(MAX_RESPONSE_BYTES));
        }
        Ok(HttpResponse {
            status,
            headers: resp_headers,
            body: buf,
        })
    }

    fn request_stream(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> std::result::Result<HttpStreamResponse, TransportError> {
        let response = self.run_ureq(method, url, headers, body, timeout)?;
        let status = response.status().as_u16();
        let resp_headers = collect_headers(response.headers());
        let reader = response.into_body().into_reader();
        Ok(HttpStreamResponse {
            status,
            headers: resp_headers,
            body: Box::new(reader),
        })
    }
}

fn collect_headers(map: &ureq::http::HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|val| (k.as_str().to_owned(), val.to_owned()))
        })
        .collect()
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// 303 always becomes GET. 301/302 drop a non-GET/HEAD body so a POST is not
/// replayed. 307/308 keep the method and body.
fn redirect_drops_body(status: u16, method: &str) -> bool {
    match status {
        303 => true,
        301 | 302 => !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD"),
        _ => false,
    }
}

/// Headers that are not credentials. A host change keeps only these, so a
/// new secret-header name cannot ride along. Same-host redirects keep the
/// caller's headers, including auth.
fn cross_host_safe(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept" | "content-type" | "user-agent" | "anthropic-version"
    )
}

pub(crate) fn headers_after_redirect(
    headers: &[(String, String)],
    same_host: bool,
) -> Vec<(String, String)> {
    if same_host {
        headers.to_vec()
    } else {
        headers
            .iter()
            .filter(|(name, _)| cross_host_safe(name))
            .cloned()
            .collect()
    }
}

fn same_host(current: &str, next: &str) -> bool {
    match (url_scheme_host(current), url_scheme_host(next)) {
        (Ok((_, a)), Ok((_, b))) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

fn url_scheme_host(url: &str) -> std::result::Result<(String, String), String> {
    let uri = url
        .parse::<ureq::http::Uri>()
        .map_err(|e| format!("invalid URL `{url}`: {e}"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| format!("URL `{url}` has no scheme"))?
        .to_ascii_lowercase();
    let host = uri
        .host()
        .ok_or_else(|| format!("URL `{url}` has no host"))?
        .to_ascii_lowercase();
    Ok((scheme, host))
}

/// Refuse a scheme downgrade and anything [`validate_endpoint_url`] refuses,
/// including non-local `http`. Called before the next request is sent.
pub(crate) fn reject_redirect(current: &str, next: &str) -> std::result::Result<(), String> {
    let (cur_scheme, _) = url_scheme_host(current)?;
    let (next_scheme, _) = url_scheme_host(next)?;
    if cur_scheme == "https" && next_scheme != "https" {
        return Err(format!(
            "refusing scheme downgrade from {current} to {next}"
        ));
    }
    validate_endpoint_url(next)
        .map(|_| ())
        .map_err(|reason| format!("refusing redirect to {next}: {reason}"))
}

pub(crate) fn resolve_redirect(
    current: &str,
    location: &str,
) -> std::result::Result<String, String> {
    let loc = location.trim();
    if loc.is_empty() || loc.contains(['\n', '\r', ' ']) {
        return Err(format!("invalid redirect Location from {current}"));
    }
    if loc.contains("://") {
        return Ok(loc.to_owned());
    }
    let base: ureq::http::Uri = current
        .parse()
        .map_err(|e| format!("invalid URL `{current}`: {e}"))?;
    let scheme = base
        .scheme_str()
        .ok_or_else(|| format!("URL `{current}` has no scheme"))?;
    let authority = base
        .authority()
        .ok_or_else(|| format!("URL `{current}` has no host"))?;
    if loc.starts_with("//") {
        return Ok(format!("{scheme}:{loc}"));
    }
    let origin = format!("{scheme}://{authority}");
    if loc.starts_with('/') {
        return Ok(format!("{origin}{loc}"));
    }
    let path = base.path();
    let dir = match path.rsplit_once('/') {
        Some((d, _)) if !d.is_empty() => d,
        _ => "",
    };
    Ok(format!("{origin}{dir}/{loc}"))
}

/// Validate that `url` is `https://` (any non-userinfo host) or `http://`
/// targeting a loopback or local address. Returns `Ok(is_http_local)`.
pub fn validate_endpoint_url(url: &str) -> std::result::Result<bool, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("endpoint URL cannot be empty".into());
    }
    let uri = trimmed
        .parse::<ureq::http::Uri>()
        .map_err(|e| format!("invalid URL `{trimmed}`: {e}"))?;
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err("URL must not contain embedded credentials".into());
    }
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| "URL must include https:// or http:// scheme".to_owned())?;
    let host = uri
        .host()
        .ok_or_else(|| "URL must include a host".to_owned())?;

    match scheme {
        "https" => Ok(false),
        "http" => {
            if is_local_or_loopback_host(host) {
                Ok(true)
            } else {
                Err(format!(
                    "plaintext http:// is only allowed for loopback or local hosts (got `{host}`)"
                ))
            }
        }
        other => Err(format!(
            "unsupported URL scheme `{other}` (expected https or local http)"
        )),
    }
}

fn is_local_or_loopback_host(host: &str) -> bool {
    let stripped = host.trim_start_matches('[').trim_end_matches(']');
    let lower = stripped.to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return true;
    }
    if let Ok(ip) = stripped.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() // 127.0.0.0/8 or RFC1918; excludes 169.254/16 link-local
            }
            IpAddr::V6(v6) => {
                if v6.is_loopback() {
                    return true;
                }
                // Unique local IPv6 (fc00::/7)
                (v6.segments()[0] & 0xfe00) == 0xfc00
            }
        };
    }
    false
}

/// Remove control characters and redact any secret tokens from error text.
pub fn sanitize_error_message(raw: &str, secrets: &[&str]) -> String {
    let mut cleaned = sanitize(raw);
    for secret in secrets {
        if !secret.is_empty() {
            cleaned = cleaned.replace(secret, "<redacted>");
        }
    }
    cleaned
}

/// Join `base_url` with `path` without duplicating overlapping version prefixes
/// such as `/v1/v1`, `/v1beta/v1beta`, `/v2/v2`, `/openai/openai`, or `/api/api`.
pub fn join_endpoint_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let rel = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };

    let first_seg = rel
        .trim_start_matches('/')
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    if !first_seg.is_empty() {
        let suffix = format!("/{first_seg}");
        if let Some(after_base) = base.strip_suffix(&suffix) {
            let _ = after_base;
            let remainder = &rel[suffix.len()..];
            if remainder.is_empty() || remainder.starts_with('/') || remainder.starts_with('?') {
                return format!("{base}{remainder}");
            }
        }
    }
    format!("{base}{rel}")
}

/// Join `base_url` with an Ollama native path (stripping a trailing `/v1` if the
/// user entered an OpenAI-style base URL for Ollama).
pub fn join_ollama_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    join_endpoint_url(base, path)
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn header_u64(headers: &[(String, String)], name: &str) -> Option<u64> {
    header_value(headers, name)?.trim().parse::<u64>().ok()
}

/// Extract rate-limit metadata from HTTP response headers.
pub fn extract_rate_limits(headers: &[(String, String)]) -> Option<RateLimitInfo> {
    let info = RateLimitInfo {
        requests_limit: header_u64(headers, "x-ratelimit-limit-requests")
            .or_else(|| header_u64(headers, "anthropic-ratelimit-requests-limit"))
            .or_else(|| header_u64(headers, "x-ratelimit-limit")),
        requests_remaining: header_u64(headers, "x-ratelimit-remaining-requests")
            .or_else(|| header_u64(headers, "anthropic-ratelimit-requests-remaining"))
            .or_else(|| header_u64(headers, "x-ratelimit-remaining")),
        tokens_limit: header_u64(headers, "x-ratelimit-limit-tokens")
            .or_else(|| header_u64(headers, "anthropic-ratelimit-tokens-limit"))
            .or_else(|| header_u64(headers, "anthropic-ratelimit-input-tokens-limit")),
        tokens_remaining: header_u64(headers, "x-ratelimit-remaining-tokens")
            .or_else(|| header_u64(headers, "anthropic-ratelimit-tokens-remaining"))
            .or_else(|| header_u64(headers, "anthropic-ratelimit-input-tokens-remaining")),
        reset_hint: header_value(headers, "x-ratelimit-reset-requests")
            .or_else(|| header_value(headers, "anthropic-ratelimit-requests-reset"))
            .or_else(|| header_value(headers, "x-ratelimit-reset-tokens"))
            .or_else(|| header_value(headers, "x-ratelimit-reset"))
            .or_else(|| header_value(headers, "retry-after"))
            .map(|s| sanitize(s.trim())),
        credits_remaining: None,
        credits_limit: None,
    };
    (!info.is_empty()).then_some(info)
}

fn is_openrouter_url(base_url: &str) -> bool {
    let Ok(uri) = base_url.trim().parse::<ureq::http::Uri>() else {
        return false;
    };
    uri.host()
        .is_some_and(|h| h.eq_ignore_ascii_case("openrouter.ai") || h.ends_with(".openrouter.ai"))
}

fn enrich_openrouter_credits(
    transport: &dyn HttpTransport,
    base_url: &str,
    api_key: &str,
    rate_limits: &mut Option<RateLimitInfo>,
) {
    if api_key.is_empty() || !is_openrouter_url(base_url) {
        return;
    }
    let Ok(uri) = base_url.trim().parse::<ureq::http::Uri>() else {
        return;
    };
    let Some(scheme) = uri.scheme_str() else {
        return;
    };
    let Some(authority) = uri.authority() else {
        return;
    };
    let key_url = format!("{scheme}://{authority}/api/v1/auth/key");
    let headers = vec![("Authorization".to_owned(), format!("Bearer {api_key}"))];
    let Ok(resp) = transport.request("GET", &key_url, &headers, None, Duration::from_secs(5))
    else {
        return;
    };
    if !(200..300).contains(&resp.status) {
        return;
    }
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&resp.body) else {
        return;
    };
    let data = json.get("data").unwrap_or(&json);
    let limit = data.get("limit").and_then(serde_json::Value::as_f64);
    let usage = data.get("usage").and_then(serde_json::Value::as_f64);
    let remaining = data
        .get("limit_remaining")
        .and_then(serde_json::Value::as_f64)
        .or_else(|| match (limit, usage) {
            (Some(l), Some(u)) => Some((l - u).max(0.0)),
            _ => None,
        });
    if limit.is_some() || remaining.is_some() {
        let info = rate_limits.get_or_insert_with(RateLimitInfo::default);
        info.credits_limit = limit;
        info.credits_remaining = remaining;
    }
}

/// Fetch and parse models from an endpoint, probing protocols when `protocol`
/// is [`EndpointProtocol::Auto`].
#[allow(clippy::too_many_arguments)]
pub fn fetch_models(
    transport: &dyn HttpTransport,
    base_url: &str,
    protocol: EndpointProtocol,
    auth_scheme: Option<&AuthScheme>,
    api_key: &str,
    api_version: Option<&str>,
    custom_models_path: Option<&str>,
    extra_headers: &BTreeMap<String, String>,
) -> Result<FetchModelsResult> {
    if let Err(reason) = validate_endpoint_url(base_url) {
        return Err(Error::refused(reason));
    }
    let secret_slice: Zeroizing<String> = Zeroizing::new(api_key.to_owned());
    let secrets = [secret_slice.as_str()];

    let candidates: Vec<EndpointProtocol> = if protocol == EndpointProtocol::Auto {
        auto_probe_order(base_url, api_version)
    } else {
        vec![protocol]
    };

    let mut last_error: Option<Error> = None;
    let mut saw_auth_error = false;

    for candidate in candidates {
        let start = Instant::now();
        match probe_single_protocol(
            transport,
            base_url,
            candidate,
            auth_scheme,
            api_key,
            api_version,
            custom_models_path,
            extra_headers,
            &secrets,
        ) {
            Ok((models, mut rate_limits)) => {
                let latency_ms = start.elapsed().as_millis() as u64;
                enrich_openrouter_credits(transport, base_url, api_key, &mut rate_limits);
                return Ok(FetchModelsResult {
                    detected_protocol: candidate,
                    models,
                    rate_limits,
                    latency_ms,
                });
            }
            Err(err) => {
                let is_auth =
                    err.message().contains("HTTP 401") || err.message().contains("HTTP 403");
                if protocol != EndpointProtocol::Auto {
                    return Err(err);
                }
                if !saw_auth_error || is_auth {
                    saw_auth_error |= is_auth;
                    last_error = Some(err);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        Error::backend("endpoint did not match any supported model listing protocol")
    }))
}

/// Probe connectivity and model listing, always returning a sanitized [`ProbeResult`].
#[allow(clippy::too_many_arguments)]
pub fn test_endpoint(
    transport: &dyn HttpTransport,
    base_url: &str,
    protocol: EndpointProtocol,
    auth_scheme: Option<&AuthScheme>,
    api_key: &str,
    api_version: Option<&str>,
    custom_models_path: Option<&str>,
    extra_headers: &BTreeMap<String, String>,
) -> ProbeResult {
    let mut secret_values: Vec<&str> = Vec::new();
    if !api_key.is_empty() {
        secret_values.push(api_key);
    }
    match fetch_models(
        transport,
        base_url,
        protocol,
        auth_scheme,
        api_key,
        api_version,
        custom_models_path,
        extra_headers,
    ) {
        Ok(res) => ProbeResult {
            reachable: true,
            detected_protocol: Some(res.detected_protocol),
            latency_ms: Some(res.latency_ms),
            model_count: res.models.len(),
            models: res.models,
            rate_limits: res.rate_limits,
            error: None,
        },
        Err(err) => ProbeResult {
            reachable: false,
            detected_protocol: None,
            latency_ms: None,
            model_count: 0,
            models: Vec::new(),
            rate_limits: None,
            error: Some(sanitize_error_message(err.message(), &secret_values)),
        },
    }
}

fn auto_probe_order(base_url: &str, api_version: Option<&str>) -> Vec<EndpointProtocol> {
    let lower = base_url.to_ascii_lowercase();
    let mut order = Vec::with_capacity(6);
    if lower.contains("anthropic") {
        order.push(EndpointProtocol::Anthropic);
    } else if lower.contains("generativelanguage.googleapis.com") || lower.contains("/v1beta") {
        order.push(EndpointProtocol::Gemini);
    } else if lower.contains("openai.azure.com") || api_version.is_some() {
        order.push(EndpointProtocol::AzureOpenAi);
    } else if lower.contains("11434") || lower.contains("ollama") {
        order.push(EndpointProtocol::Ollama);
    } else if lower.contains("cohere") {
        order.push(EndpointProtocol::Cohere);
    }

    for proto in [
        EndpointProtocol::OpenAi,
        EndpointProtocol::Anthropic,
        EndpointProtocol::Gemini,
        EndpointProtocol::Ollama,
        EndpointProtocol::AzureOpenAi,
        EndpointProtocol::Cohere,
    ] {
        if !order.contains(&proto) {
            order.push(proto);
        }
    }
    order
}

#[allow(clippy::too_many_arguments)]
fn probe_single_protocol(
    transport: &dyn HttpTransport,
    base_url: &str,
    protocol: EndpointProtocol,
    auth_scheme: Option<&AuthScheme>,
    api_key: &str,
    api_version: Option<&str>,
    custom_models_path: Option<&str>,
    extra_headers: &BTreeMap<String, String>,
    secrets: &[&str],
) -> Result<(Vec<FetchedModel>, Option<RateLimitInfo>)> {
    let url = build_models_url(base_url, protocol, api_version, custom_models_path);
    let scheme = auth_scheme
        .cloned()
        .unwrap_or_else(|| protocol.default_auth_scheme());

    let mut headers: Vec<(String, String)> = Vec::new();
    headers.push(("Accept".into(), "application/json".into()));
    scheme.apply(&mut headers, api_key);

    if protocol == EndpointProtocol::Anthropic
        && !extra_headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("anthropic-version"))
    {
        headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
    }

    for (k, v) in extra_headers {
        if !k.trim().is_empty() {
            headers.push((k.trim().to_owned(), v.clone()));
        }
    }

    let resp = transport
        .request("GET", &url, &headers, None, DEFAULT_PROBE_TIMEOUT)
        .map_err(|e| match e {
            TransportError::Refused(msg) => Error::refused(sanitize_error_message(&msg, secrets)),
            TransportError::Timeout(msg) => Error::new(
                crate::core::error::ErrorKind::Timeout,
                sanitize_error_message(&msg, secrets),
            ),
            TransportError::Network(msg) => Error::backend(sanitize_error_message(&msg, secrets)),
            TransportError::TooLarge(max) => {
                Error::format(format!("response exceeded {max} bytes"))
            }
        })?;

    let rate_limits = extract_rate_limits(&resp.headers);
    if !(200..300).contains(&resp.status) {
        let snippet = extract_error_snippet(&resp.body, secrets);
        return Err(Error::backend(format!(
            "HTTP {} from {protocol} endpoint{snippet}",
            resp.status
        )));
    }

    let models = parse_models_response(protocol, &resp.body)
        .map_err(|msg| Error::format(sanitize_error_message(&msg, secrets)))?;
    Ok((models, rate_limits))
}

fn build_models_url(
    base_url: &str,
    protocol: EndpointProtocol,
    api_version: Option<&str>,
    custom_models_path: Option<&str>,
) -> String {
    if let Some(custom) = custom_models_path.map(str::trim).filter(|s| !s.is_empty()) {
        return join_endpoint_url(base_url, custom);
    }
    match protocol {
        EndpointProtocol::OpenAi
        | EndpointProtocol::OpenAiResponses
        | EndpointProtocol::Anthropic
        | EndpointProtocol::Custom
        | EndpointProtocol::Auto => join_endpoint_url(base_url, "/v1/models"),
        EndpointProtocol::Gemini => join_endpoint_url(base_url, "/v1beta/models"),
        EndpointProtocol::Ollama => join_ollama_url(base_url, "/api/tags"),
        EndpointProtocol::AzureOpenAi => {
            let ver = api_version
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_AZURE_API_VERSION);
            join_endpoint_url(base_url, &format!("/openai/models?api-version={ver}"))
        }
        EndpointProtocol::Cohere => join_endpoint_url(base_url, "/v2/models"),
    }
}

fn extract_error_snippet(body: &[u8], secrets: &[&str]) -> String {
    if body.is_empty() {
        return String::new();
    }
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(msg) = v
            .pointer("/error/message")
            .or_else(|| v.pointer("/message"))
            .or_else(|| v.pointer("/error"))
            .and_then(serde_json::Value::as_str)
    {
        let clean = sanitize_error_message(msg.trim(), secrets);
        if !clean.is_empty() {
            return format!(": {}", truncate_chars(&clean, 160));
        }
    }
    let text = String::from_utf8_lossy(body);
    let clean = sanitize_error_message(text.trim(), secrets);
    if clean.is_empty() {
        String::new()
    } else {
        format!(": {}", truncate_chars(&clean, 160))
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, c) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// Parse a JSON model-listing payload according to `protocol` (also tolerating
/// bare JSON arrays and common provider extensions).
pub fn parse_models_response(
    protocol: EndpointProtocol,
    body: &[u8],
) -> std::result::Result<Vec<FetchedModel>, String> {
    let val: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid JSON model list: {e}"))?;

    let mut models = if let Some(arr) = val.as_array() {
        parse_model_array(arr, false)
    } else if let Some(arr) = val.get("data").and_then(serde_json::Value::as_array) {
        parse_model_array(arr, false)
    } else if let Some(arr) = val.get("models").and_then(serde_json::Value::as_array) {
        let strip_gemini = protocol == EndpointProtocol::Gemini
            || arr.iter().any(|item| {
                item.get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|n| n.starts_with("models/"))
            });
        parse_model_array(arr, strip_gemini)
    } else {
        return Err(format!(
            "unexpected model list JSON shape from {protocol} endpoint"
        ));
    };

    // Deduplicate while preserving order.
    let mut seen = std::collections::HashSet::new();
    models.retain(|m| seen.insert(m.id.clone()));
    Ok(models)
}

fn parse_model_array(arr: &[serde_json::Value], strip_models_prefix: bool) -> Vec<FetchedModel> {
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(id_str) = item.as_str() {
            let id = normalize_model_id(id_str, strip_models_prefix);
            if !id.is_empty() {
                out.push(FetchedModel {
                    id,
                    display_name: None,
                    context_window: None,
                    max_output_tokens: None,
                    owned_by: None,
                    capabilities: Vec::new(),
                });
            }
            continue;
        }

        let Some(raw_id) = item
            .get("id")
            .or_else(|| item.get("name"))
            .or_else(|| item.get("model"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let id = normalize_model_id(raw_id, strip_models_prefix);
        if id.is_empty() {
            continue;
        }

        let display_name = item
            .get("displayName")
            .or_else(|| item.get("display_name"))
            .or_else(|| item.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(sanitize)
            .filter(|name| !name.is_empty() && name != &id && name != raw_id);

        let context_window = item
            .get("context_length")
            .or_else(|| item.get("context_window"))
            .or_else(|| item.get("inputTokenLimit"))
            .or_else(|| item.get("input_token_limit"))
            .or_else(|| item.pointer("/top_provider/context_length"))
            .and_then(serde_json::Value::as_u64);

        let max_output_tokens = item
            .get("max_output_tokens")
            .or_else(|| item.get("max_completion_tokens"))
            .or_else(|| item.get("outputTokenLimit"))
            .or_else(|| item.get("output_token_limit"))
            .or_else(|| item.pointer("/top_provider/max_completion_tokens"))
            .and_then(serde_json::Value::as_u64);

        let owned_by = item
            .get("owned_by")
            .or_else(|| item.pointer("/details/family"))
            .and_then(serde_json::Value::as_str)
            .map(sanitize)
            .filter(|s| !s.is_empty());

        let capabilities = extract_capabilities(item);

        out.push(FetchedModel {
            id,
            display_name,
            context_window,
            max_output_tokens,
            owned_by,
            capabilities,
        });
    }
    out
}

fn normalize_model_id(raw: &str, strip_models_prefix: bool) -> String {
    let trimmed = sanitize(raw.trim());
    if strip_models_prefix {
        trimmed
            .strip_prefix("models/")
            .unwrap_or(&trimmed)
            .to_owned()
    } else {
        trimmed
    }
}

fn extract_capabilities(item: &serde_json::Value) -> Vec<String> {
    let mut caps = Vec::new();
    for key in [
        "capabilities",
        "supportedGenerationMethods",
        "endpoints",
        "features",
        "supported_parameters",
    ] {
        match item.get(key) {
            Some(serde_json::Value::Array(arr)) => {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        let clean = sanitize(s.trim());
                        if !clean.is_empty() && !caps.contains(&clean) {
                            caps.push(clean);
                        }
                    }
                }
            }
            Some(serde_json::Value::Object(map)) => {
                for (k, v) in map {
                    if v.as_bool() == Some(true) {
                        let clean = sanitize(k.trim());
                        if !clean.is_empty() && !caps.contains(&clean) {
                            caps.push(clean);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    caps
}

/// Zeroize a map of secret string values in place.
pub fn zeroize_string_map(map: &mut BTreeMap<String, String>) {
    for v in map.values_mut() {
        v.zeroize();
    }
    map.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization_avoids_doubled_version_segments() {
        assert_eq!(
            join_endpoint_url("https://api.openai.com/v1", "/v1/models"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            join_endpoint_url("https://api.openai.com/v1/", "/v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            join_endpoint_url("https://api.openai.com", "/v1/models"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            join_endpoint_url(
                "https://generativelanguage.googleapis.com/v1beta",
                "/v1beta/models"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        assert_eq!(
            join_ollama_url("http://127.0.0.1:11434/v1/", "/api/tags"),
            "http://127.0.0.1:11434/api/tags"
        );
    }

    #[test]
    fn url_validation_permits_https_and_local_http_only() {
        assert!(validate_endpoint_url("https://api.openai.com/v1").is_ok());
        assert!(validate_endpoint_url("http://127.0.0.1:11434").is_ok());
        assert!(validate_endpoint_url("http://localhost:1234/v1").is_ok());
        assert!(validate_endpoint_url("http://[::1]:8000/v1").is_ok());
        assert!(validate_endpoint_url("http://example.com/v1").is_err());
        assert!(validate_endpoint_url("http://169.254.169.254/latest").is_err());
        assert!(validate_endpoint_url("https://user:pass@api.openai.com/v1").is_err());
    }

    #[test]
    fn presets_include_all_seventeen_providers() {
        assert_eq!(presets().len(), 17);
        for name in [
            "openai",
            "anthropic",
            "gemini",
            "openrouter",
            "groq",
            "deepseek",
            "mistral",
            "together",
            "fireworks",
            "xai",
            "perplexity",
            "ollama",
            "lmstudio",
            "vllm",
            "litellm",
            "azure",
            "cohere",
        ] {
            assert!(find_preset(name).is_some(), "missing preset {name}");
        }
    }

    #[test]
    fn cross_host_redirect_drops_every_secret_header() {
        let headers = vec![
            ("Authorization".into(), "Bearer sk-live".into()),
            ("x-api-key".into(), "ant-secret".into()),
            ("x-goog-api-key".into(), "goog-secret".into()),
            ("api-key".into(), "azure-secret".into()),
            ("X-Custom-Auth".into(), "custom-secret".into()),
            ("X-Secret-Token".into(), "hdr-secret".into()),
            ("anthropic-version".into(), "2023-06-01".into()),
            ("Accept".into(), "application/json".into()),
        ];
        let kept = headers_after_redirect(&headers, false);
        let names: Vec<String> = kept.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
        assert_eq!(names, vec!["anthropic-version", "accept"]);
        let blob = format!("{kept:?}");
        for secret in [
            "sk-live",
            "ant-secret",
            "goog-secret",
            "azure-secret",
            "custom-secret",
            "hdr-secret",
        ] {
            assert!(!blob.contains(secret), "leaked {secret}");
        }
        assert_eq!(headers_after_redirect(&headers, true).len(), headers.len());
    }

    #[test]
    fn redirect_refuses_scheme_downgrade_and_nonlocal_http() {
        let err =
            reject_redirect("https://api.example.com/v1", "http://127.0.0.1:9/v1").unwrap_err();
        assert!(err.contains("scheme downgrade"), "{err}");
        let err = reject_redirect("http://127.0.0.1:9/v1", "http://example.com/v1").unwrap_err();
        assert!(
            err.contains("plaintext http") || err.contains("http://"),
            "{err}"
        );
        assert!(reject_redirect("https://api.example.com/v1", "https://other.example/v1").is_ok());
    }

    #[test]
    fn relative_redirect_stays_on_the_current_origin() {
        assert_eq!(
            resolve_redirect("http://127.0.0.1:9/start", "/land").unwrap(),
            "http://127.0.0.1:9/land"
        );
    }

    #[test]
    fn cross_host_redirect_mock_does_not_forward_secret_headers() {
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        // Dropping the socket after write_all RSTs on Windows before ureq reads.
        fn finish_response(sock: &mut TcpStream, bytes: &[u8]) {
            sock.write_all(bytes).expect("write mock response");
            sock.flush().expect("flush mock response");
            sock.shutdown(Shutdown::Write)
                .expect("shutdown mock write side");
            let mut drain = [0u8; 1024];
            while sock.read(&mut drain).unwrap_or(0) > 0 {}
        }

        // 127.0.0.2 is not on macOS lo0. [::1] is a different host string and still loopback.
        fn bind_landing() -> (TcpListener, String) {
            if let Ok(listener) = TcpListener::bind("127.0.0.2:0") {
                let port = listener.local_addr().expect("landing addr").port();
                return (listener, format!("http://127.0.0.2:{port}/land"));
            }
            let listener = TcpListener::bind("[::1]:0").expect("bind [::1] landing listener");
            let port = listener.local_addr().expect("landing addr").port();
            (listener, format!("http://[::1]:{port}/land"))
        }

        let hop = TcpListener::bind("127.0.0.1:0").expect("bind hop");
        let hop_addr = hop.local_addr().expect("hop addr");
        let (land, land_url) = bind_landing();
        let hop_url = format!("http://{hop_addr}/start");
        assert!(
            !same_host(&hop_url, &land_url),
            "redirect stayed on the hop host: {hop_url} -> {land_url}"
        );
        let seen_hop = Arc::new(Mutex::new(String::new()));
        let seen_land = Arc::new(Mutex::new(String::new()));
        let hop_copy = seen_hop.clone();
        let land_copy = seen_land.clone();
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {land_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        std::thread::spawn(move || {
            let (mut sock, _) = hop.accept().unwrap();
            let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            *hop_copy.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            finish_response(&mut sock, redirect.as_bytes());
        });
        std::thread::spawn(move || {
            let (mut sock, _) = land.accept().unwrap();
            let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            *land_copy.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            finish_response(
                &mut sock,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
        });

        let response = UreqTransport::new()
            .request(
                "POST",
                &hop_url,
                &[
                    ("Authorization".into(), "Bearer sk-live".into()),
                    ("x-api-key".into(), "ant-secret".into()),
                    ("x-goog-api-key".into(), "goog-secret".into()),
                    ("api-key".into(), "azure-secret".into()),
                    ("X-Custom-Auth".into(), "custom-secret".into()),
                    ("X-Secret-Token".into(), "hdr-secret".into()),
                    ("Accept".into(), "application/json".into()),
                ],
                Some(b"{}"),
                Duration::from_secs(3),
            )
            .expect("followed the local redirect");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");

        let hop_text = seen_hop.lock().unwrap().clone();
        let land_text = seen_land.lock().unwrap().clone();
        assert!(
            hop_text.contains("ant-secret"),
            "first hop lost the key: {hop_text}"
        );
        assert!(
            land_text
                .to_ascii_lowercase()
                .contains("accept: application/json"),
            "landing request missing: {land_text}"
        );
        for secret in [
            "sk-live",
            "ant-secret",
            "goog-secret",
            "azure-secret",
            "custom-secret",
            "hdr-secret",
            "Authorization",
            "x-api-key",
            "x-goog-api-key",
            "api-key",
            "X-Custom-Auth",
            "X-Secret-Token",
        ] {
            assert!(
                !land_text
                    .to_ascii_lowercase()
                    .contains(&secret.to_ascii_lowercase()),
                "cross-host landing saw {secret}: {land_text}"
            );
        }
    }
}
