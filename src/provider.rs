//! OpenAI-compatible chat-completions client, plus a native Anthropic path.
//!
//! Every OpenAI-compatible endpoint — hosted or local — is reached with the same
//! code path: POST `{base_url}/chat/completions` with an `Authorization: Bearer`
//! header. Anthropic uses `/messages` with `x-api-key` and a different body,
//! which [`Wire::Anthropic`] selects.

use std::io::Read;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::{ProviderPreset, Wire};
use crate::{Error, Result};

/// One message in a chat request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Sampling parameters. Defaults are tuned for a terminal completion: low
/// temperature so suggestions stay literal, and a small token budget since we
/// only need the remainder of a command line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature: f32,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            temperature: 0.0,
            max_tokens: 96,
            top_p: None,
            stop: Some(vec!["\n".into(), "\r".into()]),
        }
    }
}

impl Sampling {
    /// Deterministic, command-shaped completion.
    pub fn completion() -> Self {
        Self::default()
    }

    /// Slightly looser, for eager-mode exploration of several options.
    pub fn exploratory() -> Self {
        Self {
            temperature: 0.3,
            ..Self::default()
        }
    }
}

/// A chat request built from prompt material plus sampling.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub sampling: Sampling,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            sampling: Sampling::default(),
        }
    }

    pub fn with_sampling(mut self, sampling: Sampling) -> Self {
        self.sampling = sampling;
        self
    }
}

// --- request/response payloads -------------------------------------------------

#[derive(Serialize)]
struct OpenAiBody<'a> {
    model: &'a str,
    messages: &'a [Message],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a [String]>,
    stream: bool,
}

#[derive(Serialize)]
struct AnthropicBody<'a> {
    model: &'a str,
    system: Option<&'a str>,
    messages: Vec<AnthropicMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<&'a [String]>,
    stream: bool,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    error: Option<ApiErrorDetail>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    #[serde(default)]
    message: Option<String>,
}

// --- client --------------------------------------------------------------------

/// Blocking HTTP client for completion calls.
///
/// The daemon is single-threaded per request and shells never block on more than
/// one in-flight suggestion, so a blocking client keeps the hot path simple and
/// avoids an async runtime in the shell plugin path.
#[derive(Clone)]
pub struct Client {
    http: reqwest::blocking::Client,
    decision_remote_http: Option<reqwest::blocking::Client>,
    decision_local_http: Option<reqwest::blocking::Client>,
    acp: Arc<Mutex<crate::acp::Client>>,
    local_reasoning: Arc<crate::local_reasoning::Controls>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::with_timeout(Duration::from_secs(30))
    }
}

impl Client {
    pub fn with_timeout(timeout: Duration) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(3))
            .user_agent(concat!("termplete/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        let decision_remote_http = decision_http_builder(timeout, false).build().ok();
        let decision_local_http = decision_http_builder(timeout, true).build().ok();
        Self {
            http,
            decision_remote_http,
            decision_local_http,
            acp: Arc::new(Mutex::new(crate::acp::Client::new(timeout))),
            local_reasoning: Arc::new(crate::local_reasoning::Controls::default()),
        }
    }

    /// Send `req` to `preset`, using `api_key` when the provider needs one.
    ///
    /// Returns the assistant's reply text.
    pub fn complete(
        &self,
        preset: &ProviderPreset,
        model: &str,
        api_key: Option<&str>,
        req: &ChatRequest,
    ) -> Result<String> {
        if preset.wire == Wire::OpenAiAcp {
            return self.acp.lock().complete(model, req);
        }
        if preset.wire == Wire::Jev {
            return Err(Error::Provider(
                "the JEV provider supports decision requests only".into(),
            ));
        }
        let mut url = endpoint(preset);
        let mut body: serde_json::Value = match preset.wire {
            Wire::Anthropic => serde_json::to_value(anthropic_body(model, req)),
            Wire::OpenAiCompat | Wire::Ollama => serde_json::to_value(openai_body(model, req)),
            Wire::Jev | Wire::OpenAiAcp => unreachable!("unsupported provider wire was rejected"),
        }
        .map_err(|e| Error::Provider(format!("could not encode request: {e}")))?;
        apply_http_reasoning(preset, model, &mut body)?;
        let native_lmstudio = self
            .local_reasoning
            .prepare(&self.http, preset, model, api_key, &mut url, &mut body)?;
        let body = serde_json::to_vec(&body)
            .map_err(|e| Error::Provider(format!("could not encode request: {e}")))?;

        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json");
        builder = match preset.wire {
            Wire::OpenAiAcp => unreachable!("ACP requests do not use HTTP"),
            Wire::Anthropic => {
                // Anthropic uses x-api-key plus a required version header.
                if let Some(k) = api_key {
                    builder = builder.header("x-api-key", k);
                }
                builder.header("anthropic-version", "2023-06-01")
            }
            _ => {
                if let Some(k) = api_key {
                    builder = builder.header("authorization", format!("Bearer {k}"));
                }
                builder
            }
        };

        let response = builder.body(body).send().map_err(|e| classify(e, &url))?;
        let status = response.status();
        let raw = response
            .text()
            .map_err(|e| Error::Network(format!("could not read response from {url}: {e}")))?;

        if !status.is_success() {
            return Err(Error::Provider(format!(
                "{url} returned {status}: {}",
                api_error_message(&raw)
            )));
        }

        if native_lmstudio {
            return crate::local_reasoning::parse_reply(&raw);
        }

        match preset.wire {
            Wire::Anthropic => parse_anthropic(&raw),
            Wire::OpenAiCompat | Wire::Ollama => parse_openai(&raw),
            Wire::Jev => Err(Error::Provider(
                "the JEV provider supports decision requests only".into(),
            )),
            Wire::OpenAiAcp => Err(Error::Provider(
                "OpenAI ACP does not use HTTP response parsing".into(),
            )),
        }
    }

    /// Send a bounded decision request. The specialized clients do not follow
    /// redirects; loopback requests also bypass configured proxies so local
    /// context cannot be sent through one accidentally.
    pub(crate) fn complete_decision(
        &self,
        preset: &ProviderPreset,
        model: &str,
        api_key: Option<&str>,
        req: &ChatRequest,
        systemone: bool,
        systemone_body: Option<&serde_json::Value>,
    ) -> Result<DecisionReply> {
        let mut native_lmstudio = false;
        let (url, body, response_kind) = if systemone {
            let body = systemone_body
                .ok_or_else(|| Error::Provider("missing native decision request body".into()))?;
            (
                systemone_endpoint(preset),
                serde_json::to_vec(body)
                    .map_err(|_| Error::Provider("could not encode decision request".into()))?,
                DecisionResponseKind::SystemOne,
            )
        } else {
            if systemone_body.is_some() {
                return Err(Error::Provider(
                    "native decision request does not match the selected provider".into(),
                ));
            }
            match preset.wire {
                Wire::OpenAiCompat | Wire::Ollama => {
                    let mut body = openai_decision_body(model, req)?;
                    apply_http_reasoning(preset, model, &mut body)?;
                    let mut url = endpoint(preset);
                    let http = self.decision_http(&url)?;
                    native_lmstudio = self
                        .local_reasoning
                        .prepare(http, preset, model, api_key, &mut url, &mut body)?;
                    (
                        url,
                        encode_decision_body(body)?,
                        DecisionResponseKind::OpenAi,
                    )
                }
                Wire::Anthropic => {
                    let mut body = serde_json::to_value(anthropic_body(model, req))
                        .map_err(|_| Error::Provider("could not encode decision request".into()))?;
                    apply_http_reasoning(preset, model, &mut body)?;
                    (
                        endpoint(preset),
                        encode_decision_body(body)?,
                        DecisionResponseKind::Anthropic,
                    )
                }
                Wire::Jev => {
                    return Err(Error::Provider(
                        "the JEV provider supports decision requests only".into(),
                    ));
                }
                Wire::OpenAiAcp => {
                    return Err(Error::Provider(
                        "OpenAI ACP supports completion requests only".into(),
                    ));
                }
            }
        };

        let http = self.decision_http(&url)?;

        let mut builder = http.post(&url).header("content-type", "application/json");
        builder = match preset.wire {
            Wire::Anthropic => {
                if !preset.no_auth {
                    if let Some(key) = api_key {
                        builder = builder.header("x-api-key", key);
                    }
                }
                builder.header("anthropic-version", "2023-06-01")
            }
            Wire::OpenAiCompat | Wire::Ollama | Wire::Jev | Wire::OpenAiAcp => {
                if !preset.no_auth {
                    if let Some(key) = api_key {
                        builder = builder.header("authorization", format!("Bearer {key}"));
                    }
                }
                builder
            }
        };

        let response = builder
            .body(body)
            .send()
            .map_err(classify_decision_request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Provider(format!(
                "decision provider returned HTTP {status}"
            )));
        }
        let mut bytes = Vec::with_capacity(MAX_DECISION_RESPONSE_BYTES.min(4096));
        response
            .take((MAX_DECISION_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Network("could not read decision response".into()))?;
        if bytes.len() > MAX_DECISION_RESPONSE_BYTES {
            return Err(Error::Provider(
                "decision response exceeded its size limit".into(),
            ));
        }
        let raw = String::from_utf8(bytes)
            .map_err(|_| Error::Provider("decision provider returned malformed JSON".into()))?;

        if native_lmstudio {
            return crate::local_reasoning::parse_reply(&raw).map(DecisionReply::Chat);
        }

        match response_kind {
            DecisionResponseKind::SystemOne => serde_json::from_str(&raw)
                .map(DecisionReply::SystemOne)
                .map_err(|_| Error::Provider("decision provider returned malformed JSON".into())),
            DecisionResponseKind::OpenAi => parse_decision_openai(&raw).map(DecisionReply::Chat),
            DecisionResponseKind::Anthropic => {
                parse_decision_anthropic(&raw).map(DecisionReply::Chat)
            }
        }
    }

    fn decision_http(&self, url: &str) -> Result<&reqwest::blocking::Client> {
        if validate_decision_endpoint(url).is_err() {
            return Err(Error::Provider("invalid decision endpoint URL".into()));
        }
        let http = if is_local_endpoint(url) {
            self.decision_local_http.as_ref()
        } else {
            self.decision_remote_http.as_ref()
        };
        http.ok_or_else(|| Error::Provider("could not initialize decision HTTP client".into()))
    }
}

pub(crate) enum DecisionReply {
    Chat(String),
    SystemOne(serde_json::Value),
}

#[derive(Clone, Copy)]
enum DecisionResponseKind {
    OpenAi,
    Anthropic,
    SystemOne,
}

const MAX_DECISION_RESPONSE_BYTES: usize = 64 * 1024;

fn decision_http_builder(timeout: Duration, local: bool) -> reqwest::blocking::ClientBuilder {
    let builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("termplete/", env!("CARGO_PKG_VERSION")));
    if local {
        builder.no_proxy()
    } else {
        builder
    }
}

fn encode_decision_body(body: serde_json::Value) -> Result<Vec<u8>> {
    serde_json::to_vec(&body)
        .map_err(|_| Error::Provider("could not encode decision request".into()))
}

fn openai_decision_body(model: &str, req: &ChatRequest) -> Result<serde_json::Value> {
    let mut body = serde_json::to_value(openai_body(model, req))
        .map_err(|_| Error::Provider("could not encode decision request".into()))?;
    body["response_format"] = serde_json::json!({ "type": "json_object" });
    Ok(body)
}

/// Whether a decision endpoint is a loopback URL that should bypass proxies.
/// URLs with credentials, query strings, fragments, or non-HTTP schemes are
/// deliberately rejected.
pub fn is_local_endpoint(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if validate_parsed_endpoint(&parsed).is_err() {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn validate_decision_endpoint(url: &str) -> std::result::Result<(), ()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| ())?;
    validate_parsed_endpoint(&parsed)
}

fn validate_parsed_endpoint(parsed: &reqwest::Url) -> std::result::Result<(), ()> {
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(());
    }
    Ok(())
}

fn systemone_endpoint(preset: &ProviderPreset) -> String {
    let base = preset.base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/systemone")
    } else {
        format!("{base}/v1/systemone")
    }
}

fn classify_decision_request(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Network("decision request timed out".into())
    } else if error.is_connect() {
        Error::Network("could not connect to decision provider".into())
    } else {
        Error::Network("decision request failed".into())
    }
}

#[derive(Deserialize)]
struct DecisionOpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

fn parse_decision_openai(raw: &str) -> Result<String> {
    let parsed: DecisionOpenAiResponse = serde_json::from_str(raw)
        .map_err(|_| Error::Provider("decision provider returned malformed JSON".into()))?;
    parsed
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .ok_or_else(|| Error::Provider("decision provider returned no content".into()))
}

#[derive(Deserialize)]
struct DecisionAnthropicResponse {
    content: Vec<AnthropicContent>,
}

fn parse_decision_anthropic(raw: &str) -> Result<String> {
    let parsed: DecisionAnthropicResponse = serde_json::from_str(raw)
        .map_err(|_| Error::Provider("decision provider returned malformed JSON".into()))?;
    let content = parsed
        .content
        .into_iter()
        .filter_map(|block| block.text)
        .collect::<Vec<_>>()
        .join("");
    if content.is_empty() {
        return Err(Error::Provider(
            "decision provider returned no content".into(),
        ));
    }
    Ok(content)
}

fn endpoint(preset: &ProviderPreset) -> String {
    let base = preset.base_url.trim_end_matches('/');
    match preset.wire {
        // Anthropic's base already ends in /v1; OpenAI-compatible bases do too.
        Wire::Anthropic => format!("{base}/messages"),
        Wire::OpenAiCompat | Wire::Ollama => format!("{base}/chat/completions"),
        Wire::Jev => format!("{base}/systemone"),
        Wire::OpenAiAcp => unreachable!("ACP requests do not use HTTP endpoints"),
    }
}

/// Apply only reasoning controls documented for the selected HTTP provider and
/// model family. Unknown OpenAI-compatible servers are left alone because they
/// do not share a standard reasoning-control parameter.
fn apply_http_reasoning(
    preset: &ProviderPreset,
    model: &str,
    body: &mut serde_json::Value,
) -> Result<()> {
    configure_http_reasoning(preset, model, body)?;
    let effort = body
        .get("reasoning_effort")
        .or_else(|| body.pointer("/reasoning/effort"))
        .or_else(|| body.pointer("/output_config/effort"))
        .and_then(serde_json::Value::as_str);
    if matches!(effort, Some("low" | "minimal")) {
        // Required thinking shares the token budget with the command answer.
        if let Some(fields) = body.as_object_mut() {
            fields.remove("stop");
            fields.remove("stop_sequences");
            for field in ["max_tokens", "max_completion_tokens"] {
                if let Some(limit) = fields.get(field).and_then(serde_json::Value::as_u64) {
                    fields.insert(field.into(), serde_json::json!(limit.max(512)));
                }
            }
        }
    }
    Ok(())
}

fn configure_http_reasoning(
    preset: &ProviderPreset,
    model: &str,
    body: &mut serde_json::Value,
) -> Result<()> {
    let model = model.to_ascii_lowercase();
    let model_id = model.rsplit('/').next().unwrap_or(&model);

    if preset.wire == Wire::Anthropic {
        let is_claude_5 = has_model_family(
            model_id,
            &[
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-fable-5",
                "claude-mythos-5",
            ],
        );
        if has_model_family(
            model_id,
            &[
                "claude-fable-5",
                "claude-mythos-5",
                "claude-mythos-preview",
                "claude-opus-5-5",
                "claude-opus-5.5",
            ],
        ) {
            body["output_config"] = serde_json::json!({"effort": "low"});
        } else if has_model_family(model_id, &["claude-sonnet-5-5", "claude-sonnet-5.5"]) {
            body["thinking"] = serde_json::json!({"type": "between_tools"});
            body["output_config"] = serde_json::json!({"effort": "low"});
        } else if is_claude_5
            || has_model_family(
                model_id,
                &[
                    "claude-opus-4",
                    "claude-sonnet-4",
                    "claude-haiku-4",
                    "claude-3-7-sonnet",
                ],
            )
        {
            body["thinking"] = serde_json::json!({"type": "disabled"});
            if is_claude_5 {
                body["output_config"] = serde_json::json!({"effort": "low"});
            }
        }
        if is_claude_5 {
            // Claude 5 rejects non-default sampling parameters.
            if let Some(fields) = body.as_object_mut() {
                fields.remove("temperature");
                fields.remove("top_p");
            }
        }
        return Ok(());
    }

    if !matches!(preset.wire, Wire::OpenAiCompat) {
        // Ollama and LM Studio use capability-driven policies in local_reasoning.
        return Ok(());
    }

    let host = reqwest::Url::parse(&preset.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let is_local = host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host.starts_with("192.168.")
        || preset.name.eq_ignore_ascii_case("lmstudio");
    if is_local {
        return Ok(());
    }

    let is_openrouter = provider_matches(preset, &host, "openrouter", &["openrouter.ai"]);
    let is_openai = provider_matches(preset, &host, "openai", &["openai.com"]);
    let is_groq = provider_matches(preset, &host, "groq", &["groq.com"]);
    let is_together = provider_matches(preset, &host, "together", &["together.ai"]);
    let is_deepseek = provider_matches(preset, &host, "deepseek", &["deepseek.com"]);
    let is_mistral = provider_matches(preset, &host, "mistral", &["mistral.ai"]);
    let is_gemini = provider_matches(
        preset,
        &host,
        "gemini",
        &["generativelanguage.googleapis.com"],
    );
    let is_xai = provider_matches(preset, &host, "xai", &["x.ai"]);
    let is_fireworks = provider_matches(preset, &host, "fireworks", &["fireworks.ai"]);

    if is_openrouter {
        if is_high_only_model(model_id) {
            return Err(high_only_reasoning_error(&model));
        }
        if has_model_family(
            model_id,
            &[
                "claude-fable-5",
                "claude-mythos-5",
                "claude-mythos-preview",
                "claude-opus-5.5",
                "claude-opus-5-5",
            ],
        ) {
            body["reasoning"] = serde_json::json!({"effort": "low"});
        } else if let Some(effort) = openai_minimum_effort(model_id) {
            body["reasoning"] = serde_json::json!({"effort": effort});
        } else {
            body["reasoning"] = serde_json::json!({"enabled": false});
        }
        return Ok(());
    }

    if is_deepseek {
        // DeepSeek's Chat Completions API accepts this native thinking switch.
        body["thinking"] = serde_json::json!({"type": "disabled"});
        return Ok(());
    }

    if is_groq {
        if has_model_family(model_id, &["qwen3.6", "qwen3.8"]) {
            body["reasoning_effort"] = serde_json::json!("none");
        } else if has_model_family(model_id, &["gpt-oss-20b", "gpt-oss-120b"]) {
            body["reasoning_effort"] = serde_json::json!("low");
        }
        return Ok(());
    }

    if is_together {
        if has_model_family(model_id, &["qwen3.5"]) {
            body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
        }
        return Ok(());
    }

    if is_mistral {
        if has_model_family(model_id, &["mistral-small-latest", "mistral-medium-3-5"]) {
            body["reasoning_effort"] = serde_json::json!("none");
        } else if has_model_family(model_id, &["zai-glm-5-3"]) {
            body["reasoning_effort"] = serde_json::json!("low");
        }
        return Ok(());
    }

    if is_gemini {
        if has_model_family(model_id, &["gemini-2.5-pro"]) {
            body["reasoning_effort"] = serde_json::json!("minimal");
        } else if has_model_family(model_id, &["gemini-2.5-flash", "gemini-2.5-flash-lite"]) {
            body["reasoning_effort"] = serde_json::json!("none");
        } else if has_model_family(model_id, &["gemini-3"]) {
            // Maps to Low on Pro, Minimal on Flash models.
            body["reasoning_effort"] = serde_json::json!("minimal");
            if let Some(fields) = body.as_object_mut() {
                fields.remove("temperature");
            }
        }
        return Ok(());
    }

    if is_xai {
        if has_model_family(model_id, &["grok-4.3", "grok-4-fast", "grok-4.1-fast"]) {
            body["reasoning_effort"] = serde_json::json!("none");
        } else if has_model_family(model_id, &["grok-4.5", "grok-4.6", "grok-4.7", "grok-4.20"]) {
            // These xAI models cannot turn reasoning off; low is the minimum.
            body["reasoning_effort"] = serde_json::json!("low");
            if let Some(fields) = body.as_object_mut() {
                fields.remove("stop");
            }
        }
        return Ok(());
    }

    if is_fireworks {
        if has_model_family(model_id, &["qwen3", "deepseek-v3p1", "deepseek-v3p2"]) {
            body["reasoning_effort"] = serde_json::json!("none");
        }
        return Ok(());
    }

    if is_openai || is_openai_reasoning_model(model_id) {
        if is_high_only_model(model_id) {
            return Err(high_only_reasoning_error(&model));
        }
        if let Some(effort) = openai_minimum_effort(model_id) {
            body["reasoning_effort"] = serde_json::json!(effort);
            if let Some(fields) = body.as_object_mut() {
                fields.remove("temperature");
                fields.remove("top_p");
                fields.remove("stop");
                if let Some(token_limit) = fields.remove("max_tokens") {
                    fields.insert("max_completion_tokens".into(), token_limit);
                }
            }
        }
    }
    Ok(())
}

fn provider_matches(preset: &ProviderPreset, host: &str, name: &str, domains: &[&str]) -> bool {
    preset.name.eq_ignore_ascii_case(name)
        || domains
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

fn has_model_family(model: &str, families: &[&str]) -> bool {
    families.iter().any(|family| model.starts_with(family))
}

fn is_high_only_model(model: &str) -> bool {
    has_model_family(model, &["o1-pro", "o3-pro"])
        || (model.starts_with("gpt-5") && model.contains("-pro"))
}

fn openai_minimum_effort(model: &str) -> Option<&'static str> {
    if model.contains("chat-latest") {
        return None;
    }
    if has_model_family(
        model,
        &[
            "gpt-6.1-astra",
            "gpt-6.1-sol",
            "gpt-6-astra",
            "o1",
            "o3",
            "o4",
            "gpt-oss",
        ],
    ) || (model.starts_with("gpt-5") && model.contains("codex"))
    {
        Some("low")
    } else if has_model_family(
        model,
        &[
            "gpt-6-luna",
            "gpt-6-sol",
            "gpt-5.1",
            "gpt-5.2",
            "gpt-5.4",
            "gpt-5.5",
            "gpt-5.6",
        ],
    ) {
        Some("none")
    } else if model == "gpt-5" || model.starts_with("gpt-5-") {
        Some("minimal")
    } else {
        None
    }
}

fn is_openai_reasoning_model(model: &str) -> bool {
    has_model_family(model, &["gpt-5", "gpt-6", "gpt-oss", "o1", "o3", "o4"])
}

fn high_only_reasoning_error(model: &str) -> Error {
    Error::Provider(format!(
        "model `{model}` cannot disable reasoning or use Low; choose a model that supports lower effort"
    ))
}

fn openai_body<'a>(model: &'a str, req: &'a ChatRequest) -> OpenAiBody<'a> {
    OpenAiBody {
        model,
        messages: &req.messages,
        temperature: req.sampling.temperature,
        max_tokens: req.sampling.max_tokens,
        top_p: req.sampling.top_p,
        stop: req.sampling.stop.as_deref(),
        stream: false,
    }
}

fn anthropic_body<'a>(model: &'a str, req: &'a ChatRequest) -> AnthropicBody<'a> {
    let system = req
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .map(|m| m.content.as_str());
    let messages: Vec<AnthropicMessage<'_>> = req
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|m| AnthropicMessage {
            role: match m.role {
                Role::Assistant => "assistant",
                _ => "user",
            },
            content: &m.content,
        })
        .collect();
    AnthropicBody {
        model,
        system,
        messages,
        temperature: req.sampling.temperature,
        max_tokens: req.sampling.max_tokens,
        top_p: req.sampling.top_p,
        stop_sequences: req.sampling.stop.as_deref(),
        stream: false,
    }
}

fn parse_openai(raw: &str) -> Result<String> {
    let parsed: OpenAiResponse = serde_json::from_str(raw).map_err(|e| {
        Error::Provider(format!(
            "unexpected OpenAI-shaped response: {e}; body: {}",
            truncate_body(raw)
        ))
    })?;
    parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .ok_or_else(|| Error::Provider("response contained no choices".into()))
}

fn parse_anthropic(raw: &str) -> Result<String> {
    let parsed: AnthropicResponse = serde_json::from_str(raw).map_err(|e| {
        Error::Provider(format!(
            "unexpected Anthropic-shaped response: {e}; body: {}",
            truncate_body(raw)
        ))
    })?;
    Ok(parsed
        .content
        .into_iter()
        .filter_map(|c| c.text)
        .collect::<Vec<_>>()
        .join(""))
}

fn api_error_message(raw: &str) -> String {
    match serde_json::from_str::<ApiErrorBody>(raw) {
        Ok(body) => body
            .error
            .and_then(|e| e.message)
            .or(body.message)
            .unwrap_or_else(|| truncate_body(raw)),
        Err(_) => truncate_body(raw),
    }
}

fn truncate_body(raw: &str) -> String {
    let t = raw.trim();
    if t.chars().count() <= 300 {
        return t.to_string();
    }
    let head: String = t.chars().take(300).collect();
    format!("{head}…")
}

fn classify(e: reqwest::Error, url: &str) -> Error {
    if e.is_timeout() {
        Error::Network(format!("request to {url} timed out"))
    } else if e.is_connect() {
        Error::Network(format!(
            "could not reach {url} — is the provider running? ({e})"
        ))
    } else {
        Error::Network(format!("request to {url} failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(wire: Wire) -> ProviderPreset {
        crate::config::builtin_providers()
            .into_iter()
            .find(|p| p.wire == wire)
            .unwrap_or_else(|| panic!("no preset for {wire:?}"))
    }

    #[test]
    fn openai_endpoint_has_no_double_slash() {
        let mut p = preset(Wire::OpenAiCompat);
        p.base_url = "https://api.openai.com/v1/".into();
        assert_eq!(endpoint(&p), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn ollama_uses_openai_path() {
        let p = preset(Wire::Ollama);
        assert_eq!(endpoint(&p), "http://127.0.0.1:11434/v1/chat/completions");
    }

    #[test]
    fn decision_chat_fallback_requests_json_without_newline_stops() {
        let req = ChatRequest::new(
            "model",
            vec![Message::system("classify"), Message::user("data")],
        )
        .with_sampling(Sampling {
            temperature: 0.0,
            max_tokens: 128,
            top_p: None,
            stop: None,
        });
        let body = openai_decision_body("model", &req).unwrap();
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["stream"], false);
        assert!(body.get("stop").is_none());
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 128);
    }

    #[test]
    fn systemone_endpoint_adds_v1_when_preset_base_omits_it() {
        let mut p = preset(Wire::Ollama);
        p.base_url = "http://127.0.0.1:11434".into();
        assert_eq!(
            systemone_endpoint(&p),
            "http://127.0.0.1:11434/v1/systemone"
        );
    }

    #[test]
    fn anthropic_endpoint_uses_messages() {
        let p = preset(Wire::Anthropic);
        assert!(endpoint(&p).ends_with("/messages"), "{}", endpoint(&p));
    }

    #[test]
    fn parses_openai_reply() {
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":" --files"}}]}"#;
        assert_eq!(parse_openai(raw).unwrap(), " --files");
    }

    #[test]
    fn parses_null_content_as_error() {
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":null}}]}"#;
        assert!(parse_openai(raw).is_err());
    }

    #[test]
    fn parses_anthropic_blocks() {
        let raw = r#"{"content":[{"type":"text","text":"foo"},{"type":"text","text":"bar"}]}"#;
        assert_eq!(parse_anthropic(raw).unwrap(), "foobar");
    }

    #[test]
    fn extracts_nested_api_error_message() {
        let raw =
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#;
        assert!(api_error_message(raw).contains("Incorrect API key"));
    }

    #[test]
    fn extracts_openai_style_error_message() {
        let raw = r#"{"message":"model not found"}"#;
        assert_eq!(api_error_message(raw), "model not found");
    }

    #[test]
    fn non_json_error_body_is_truncated_not_panicked() {
        let raw = "x".repeat(1000);
        let msg = api_error_message(&raw);
        assert!(msg.chars().count() <= 301);
    }

    #[test]
    fn anthropic_body_lifts_system_out_of_messages() {
        let req = ChatRequest::new("claude", vec![Message::system("sys"), Message::user("hi")]);
        let body = anthropic_body("claude", &req);
        assert_eq!(body.system, Some("sys"));
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
    }

    #[test]
    fn stop_sequences_are_sent_as_strings() {
        let req = ChatRequest::new("m", vec![Message::user("hi")]);
        let body = openai_body("m", &req);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["stop"][0], "\n");
        assert_eq!(json["stream"], false);
    }
}
