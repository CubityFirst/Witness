//! A small client for OpenAI-compatible Chat Completions APIs (OpenAI
//! itself, and local servers such as Ollama, LM Studio or llama.cpp that
//! speak the same protocol). Used only by the opt-in AI assistant
//! (`service/ai.rs`); nothing here runs unless the user turned it on.
//!
//! - **Requests**: `POST {base}/chat/completions` and `GET {base}/models`,
//!   JSON through serde_json, `Authorization: Bearer <key>` only when a key
//!   is stored (local servers usually need none). No temperature or token
//!   limits are sent: newer models reject non-default values.
//! - **Transport seam**: [`AiTransport`] carries one request and returns
//!   the status + body, so request building, parsing and error mapping are
//!   unit-tested with a fake. [`UreqTransport`] is the real one: the
//!   updater's SChannel + system-proxy agent (loopback goes direct),
//!   redirects off (a key is never forwarded anywhere else) and HTTPS-only
//!   for non-loopback hosts.
//! - **Secrets**: the key lives in an [`ApiKey`] whose `Debug` is redacted
//!   and whose bytes are zeroed on drop. Error texts never include headers
//!   or URLs, and server messages are scrubbed of the key and of anything
//!   that looks like one before they reach a toast or the log.

use serde::Deserialize;
use serde_json::json;
use std::fmt;
use std::time::Duration;

/// Overall deadline of a chat completion. Generous: a local model on a CPU
/// can take minutes for the notes of a long meeting.
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(600);
/// Overall deadline of the connection test and the model list (a cold
/// local model may need a while to load for the test).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// The most of an error body kept for the message (it is scrubbed first).
const ERROR_DETAIL_CHARS: usize = 200;
/// A model list keeps at most this many ids, each at most this long.
pub const MAX_MODELS: usize = 1000;
const MAX_MODEL_ID_BYTES: usize = 256;
/// The `User-Agent` of AI requests (no version, no platform).
pub const AI_USER_AGENT: &str = "Witness";

// ---------------------------------------------------------------------------
// API key

/// An API key. Never printed: `Debug` is redacted, and the bytes are
/// overwritten when it is dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// A trimmed, non-empty key (None for blank input).
    pub fn new(text: &str) -> Option<ApiKey> {
        let text = text.trim();
        (!text.is_empty()).then(|| ApiKey(text.to_string()))
    }

    /// The secret itself — only for the Authorization header and the
    /// credential store.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl Drop for ApiKey {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// Overwrite a string's bytes in place before it is dropped or reused
/// (best effort: copies made elsewhere — the HTTP library's buffers, the
/// OS — are out of reach).
pub fn wipe(text: &mut String) {
    let len = text.len();
    text.clear();
    // Writes `len` NULs over the same allocation (capacity ≥ len).
    text.extend(std::iter::repeat_n('\0', len));
    // Keep the stores from being optimised away as dead.
    std::hint::black_box(text.as_str());
    text.clear();
}

/// `text` cut to at most `max` bytes at a character boundary.
pub fn truncate_bytes(text: &mut String, max: usize) {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
}

// ---------------------------------------------------------------------------
// Base URL

/// A validated API root: `scheme://host[:port][/path]` without a trailing
/// slash, query, fragment or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    url: String,
    host: String,
    https: bool,
}

impl BaseUrl {
    pub fn parse(raw: &str) -> Result<BaseUrl, String> {
        let url = raw.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err("Enter the service's base URL.".into());
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or("The base URL must start with https:// (or http:// for a local server).")?;
        let https = if scheme.eq_ignore_ascii_case("https") {
            true
        } else if scheme.eq_ignore_ascii_case("http") {
            false
        } else {
            return Err(
                "The base URL must start with https:// (or http:// for a local server).".into(),
            );
        };
        if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("The base URL cannot contain spaces.".into());
        }
        if rest.contains(['?', '#']) {
            return Err("The base URL cannot have a query (?…) or fragment (#…).".into());
        }
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.contains('@') {
            return Err("Put the API key in its own field, not in the URL.".into());
        }
        let host = host_of(authority).ok_or("The base URL has no host name.")?;
        if !https && !crate::service::updater::is_loopback_host(&host) {
            return Err(
                "Use https:// — plain http:// is only allowed for localhost, 127.0.0.1 or [::1]."
                    .into(),
            );
        }
        Ok(BaseUrl {
            url: format!("{}://{rest}", if https { "https" } else { "http" }),
            host,
            https,
        })
    }

    /// `{base}/{path}`.
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.url, path.trim_start_matches('/'))
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn is_https(&self) -> bool {
        self.https
    }

    /// The host is this machine (the data never leaves it).
    pub fn is_local(&self) -> bool {
        crate::service::updater::is_loopback_host(&self.host)
    }

    pub fn as_str(&self) -> &str {
        &self.url
    }
}

/// `host[:port]` → host (an IPv6 literal keeps its brackets); None when
/// the host is empty or the port is not a number.
fn host_of(authority: &str) -> Option<String> {
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (inner, after) = v6.split_once(']')?;
        let port = match after {
            "" => None,
            rest => Some(rest.strip_prefix(':')?),
        };
        (format!("[{inner}]"), port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_string(), Some(port)),
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty() || host == "[]" {
        return None;
    }
    if let Some(port) = port {
        port.parse::<u16>().ok()?;
    }
    Some(host)
}

/// Validate (and normalise) a base URL as typed in Settings.
pub fn validate_base_url(raw: &str) -> Result<String, String> {
    BaseUrl::parse(raw).map(|base| base.url)
}

// ---------------------------------------------------------------------------
// Transport seam

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One HTTP request. `Debug` never shows the key.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    /// The host `url` points at (proxy choice; loopback goes direct).
    pub host: String,
    /// Refuse anything but HTTPS (every non-loopback base).
    pub https_only: bool,
    pub bearer: Option<ApiKey>,
    /// JSON body (POST only).
    pub body: Option<String>,
    pub timeout: Duration,
}

/// Any response that arrived, whatever its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Why no response arrived. Messages never include the URL or headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    /// DNS, refused/reset connections, TLS, proxies.
    Network(String),
}

/// Carries one request. The real one is [`UreqTransport`]; tests use fakes.
pub trait AiTransport: Send + Sync + 'static {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The real transport: ureq with the updater's agent setup.
#[derive(Debug, Default, Clone, Copy)]
pub struct UreqTransport;

/// Response bodies are read up to this size (a chat reply is far smaller).
const MAX_BODY_BYTES: u64 = 8 << 20;

impl AiTransport for UreqTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        use std::io::Read;

        let agent = crate::service::updater::agent_builder("ai", &[request.host.as_str()])
            .map_err(|e| TransportError::Network(format!("{e:#}")))?
            // A redirect would carry the request (and its key) elsewhere;
            // API roots don't redirect, so a 3xx is reported instead.
            .redirects(0)
            .https_only(request.https_only)
            // A neutral user agent: no version (PRIVACY.md lists it).
            .user_agent(AI_USER_AGENT)
            .build();
        let mut call = match request.method {
            Method::Get => agent.get(&request.url),
            Method::Post => agent.post(&request.url),
        }
        .timeout(request.timeout)
        .set("Accept", "application/json");
        let mut authorization = String::new();
        if let Some(key) = &request.bearer {
            authorization = format!("Bearer {}", key.expose());
            call = call.set("Authorization", &authorization);
        }
        let result = match &request.body {
            Some(body) => call
                .set("Content-Type", "application/json")
                .send_string(body),
            None => call.call(),
        };
        wipe(&mut authorization);
        let response = match result {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(transport)) => return Err(transport_error(&transport)),
        };
        let status = response.status();
        let mut body = String::new();
        response
            .into_reader()
            .take(MAX_BODY_BYTES)
            .read_to_string(&mut body)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                    TransportError::Timeout
                }
                _ => TransportError::Network(format!("reading the response failed: {e}")),
            })?;
        Ok(HttpResponse { status, body })
    }
}

/// ureq's transport errors print their URL; rebuild them without it.
fn transport_error(transport: &ureq::Transport) -> TransportError {
    let timed_out = std::error::Error::source(transport)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        });
    if timed_out {
        return TransportError::Timeout;
    }
    let mut message = transport.kind().to_string();
    if let Some(detail) = transport.message() {
        message.push_str(&format!(": {detail}"));
    }
    if let Some(source) = std::error::Error::source(transport) {
        message.push_str(&format!(": {source}"));
    }
    TransportError::Network(message)
}

// ---------------------------------------------------------------------------
// Errors

/// Everything that can go wrong, worded for a toast. Never carries the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    /// The master switch is off.
    Disabled,
    InvalidConfig(String),
    /// 401 / 403.
    Unauthorized(u16),
    /// 404.
    NotFound,
    /// 429.
    RateLimited,
    /// 5xx.
    Server(u16),
    /// 3xx with redirects off.
    Redirected(u16),
    /// Any other 4xx, with the server's (scrubbed) explanation.
    Rejected {
        status: u16,
        detail: Option<String>,
    },
    Timeout,
    Network(String),
    /// A 2xx whose body is not a Chat Completions / models response.
    BadResponse(String),
    /// A well-formed reply without any text.
    Empty,
    /// The credential store could not be read.
    Credentials(String),
    /// The stored key belongs to another host than the configured base URL.
    KeyForOtherHost {
        key_host: String,
        host: String,
    },
    /// One job reached its request budget (a model whose replies kept it
    /// from finishing).
    TooManyRequests,
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::Disabled => f.write_str("The AI service is turned off in Settings."),
            AiError::InvalidConfig(why) => write!(f, "AI service settings: {why}"),
            AiError::Unauthorized(code) => write!(
                f,
                "The AI service rejected the API key ({code}). Check the key in Settings → AI assistant."
            ),
            AiError::NotFound => f.write_str(
                "The AI service doesn't know this endpoint or model (404). Check the base URL and the model.",
            ),
            AiError::RateLimited => f.write_str(
                "The AI service is rate-limiting requests or the account is out of credit (429). Try again later.",
            ),
            AiError::Server(code) => {
                write!(f, "The AI service had a server error ({code}). Try again later.")
            }
            AiError::Redirected(code) => write!(
                f,
                "The AI service redirected the request ({code}). Check the base URL."
            ),
            AiError::Rejected { status, detail } => match detail {
                Some(detail) => write!(f, "The AI service refused the request ({status}): {detail}"),
                None => write!(f, "The AI service refused the request ({status})."),
            },
            AiError::Timeout => f.write_str("The AI service did not answer in time."),
            AiError::Network(why) => write!(f, "Could not reach the AI service: {why}"),
            AiError::BadResponse(why) => {
                write!(f, "The AI service sent a reply Witness could not read ({why}).")
            }
            AiError::Empty => f.write_str("The AI service sent an empty reply."),
            AiError::Credentials(why) => write!(f, "Could not read the API key: {why}"),
            AiError::KeyForOtherHost { key_host, host } => write!(
                f,
                "The saved API key is for {key_host}, not {host}. Enter the key for {host} in Settings \u{2192} AI assistant."
            ),
            AiError::TooManyRequests => f.write_str(
                "The AI service\u{2019}s replies kept the job from finishing; it was stopped.",
            ),
        }
    }
}

impl std::error::Error for AiError {}

/// Map a non-2xx response. `key` is scrubbed from the server's message.
pub fn map_status(status: u16, body: &str, key: Option<&ApiKey>) -> AiError {
    match status {
        401 | 403 => AiError::Unauthorized(status),
        404 => AiError::NotFound,
        429 => AiError::RateLimited,
        300..=399 => AiError::Redirected(status),
        500..=599 => AiError::Server(status),
        _ => AiError::Rejected {
            status,
            detail: error_detail(body).map(|detail| scrub(&detail, key)),
        },
    }
}

/// `{"error": {"message": "…"}}` (OpenAI, most servers) or
/// `{"error": "…"}` (Ollama) or `{"message": "…"}`.
fn error_detail(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = value
        .get("error")
        .and_then(|e| e.get("message").or(Some(e)))
        .and_then(|m| m.as_str())
        .or_else(|| value.get("message").and_then(|m| m.as_str()))?;
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    (!message.is_empty()).then_some(message)
}

/// Remove the key, and anything shaped like a key (`sk-…`, long tokens
/// without spaces), from a server message, then cap its length.
pub fn scrub(text: &str, key: Option<&ApiKey>) -> String {
    let mut text = text.to_string();
    if let Some(key) = key {
        if !key.expose().is_empty() {
            text = text.replace(key.expose(), "[redacted]");
        }
    }
    let words: Vec<String> = text
        .split(' ')
        .map(|word| {
            let bare = word.trim_matches(|c: char| !c.is_alphanumeric());
            let keyish = bare.starts_with("sk-")
                || bare.starts_with("sk_")
                || (bare.len() >= 24
                    && bare
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_*.".contains(c))
                    && bare.chars().any(|c| c.is_ascii_digit()));
            if keyish {
                "[redacted]".to_string()
            } else {
                word.to_string()
            }
        })
        .collect();
    let joined = words.join(" ");
    if joined.chars().count() > ERROR_DETAIL_CHARS {
        let cut: String = joined.chars().take(ERROR_DETAIL_CHARS).collect();
        format!("{}…", cut.trim_end())
    } else {
        joined
    }
}

// ---------------------------------------------------------------------------
// Requests and replies

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::User,
            content: content.into(),
        }
    }
}

/// The Chat Completions request body.
pub fn chat_body(model: &str, messages: &[ChatMessage]) -> String {
    let messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            json!({
                "role": match m.role { Role::System => "system", Role::User => "user" },
                "content": m.content,
            })
        })
        .collect();
    json!({ "model": model, "messages": messages, "stream": false }).to_string()
}

#[derive(Deserialize)]
struct ChatReply {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatReplyMessage,
}

#[derive(Deserialize)]
struct ChatReplyMessage {
    /// A string, or (some servers) an array of `{type, text}` parts.
    #[serde(default)]
    content: Option<serde_json::Value>,
}

/// The first choice's text, minus a leading `<think>…</think>` block that
/// reasoning models served locally sometimes include.
pub fn parse_chat_reply(body: &str) -> Result<String, AiError> {
    let reply: ChatReply = serde_json::from_str(body)
        .map_err(|_| AiError::BadResponse("not a chat completion".into()))?;
    let choice = reply.choices.into_iter().next().ok_or(AiError::Empty)?;
    let text = match choice.message.content {
        Some(serde_json::Value::String(text)) => text,
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    let text = strip_thinking(&text).trim().to_string();
    if text.is_empty() {
        return Err(AiError::Empty);
    }
    Ok(text)
}

fn strip_thinking(text: &str) -> &str {
    let trimmed = text.trim_start();
    if trimmed.starts_with("<think>") {
        if let Some(end) = trimmed.find("</think>") {
            return &trimmed[end + "</think>".len()..];
        }
    }
    text
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// `GET /models` → sorted, de-duplicated model ids.
pub fn parse_models(body: &str) -> Result<Vec<String>, AiError> {
    let list: ModelList =
        serde_json::from_str(body).map_err(|_| AiError::BadResponse("not a model list".into()))?;
    let mut ids: Vec<String> = list
        .data
        .into_iter()
        .map(|m| m.id.trim().to_string())
        .filter(|id| {
            !id.is_empty() && id.len() <= MAX_MODEL_ID_BYTES && !id.contains(char::is_control)
        })
        .take(MAX_MODELS)
        .collect();
    ids.sort_by_key(|id| id.to_lowercase());
    ids.dedup();
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Client

/// One configured service: a base URL, an optional key and a model.
pub struct AiClient<'a> {
    transport: &'a dyn AiTransport,
    base: BaseUrl,
    key: Option<ApiKey>,
}

impl<'a> AiClient<'a> {
    pub fn new(
        transport: &'a dyn AiTransport,
        base_url: &str,
        key: Option<ApiKey>,
    ) -> Result<Self, AiError> {
        let base = BaseUrl::parse(base_url).map_err(AiError::InvalidConfig)?;
        Ok(AiClient {
            transport,
            base,
            key,
        })
    }

    pub fn base(&self) -> &BaseUrl {
        &self.base
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
        timeout: Duration,
    ) -> HttpRequest {
        HttpRequest {
            method,
            url: self.base.endpoint(path),
            host: self.base.host().to_string(),
            https_only: !self.base.is_local(),
            bearer: self.key.clone(),
            body,
            timeout,
        }
    }

    fn send(&self, request: &HttpRequest) -> Result<String, AiError> {
        let response = self.transport.send(request).map_err(|e| match e {
            TransportError::Timeout => AiError::Timeout,
            TransportError::Network(why) => AiError::Network(scrub(&why, self.key.as_ref())),
        })?;
        if !(200..300).contains(&response.status) {
            return Err(map_status(
                response.status,
                &response.body,
                self.key.as_ref(),
            ));
        }
        Ok(response.body)
    }

    /// One chat completion → the reply text.
    pub fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        timeout: Duration,
    ) -> Result<String, AiError> {
        if model.trim().is_empty() {
            return Err(AiError::InvalidConfig("choose a model".into()));
        }
        let request = self.request(
            Method::Post,
            "chat/completions",
            Some(chat_body(model.trim(), messages)),
            timeout,
        );
        parse_chat_reply(&self.send(&request)?)
    }

    /// The models the service offers.
    pub fn list_models(&self) -> Result<Vec<String>, AiError> {
        let request = self.request(Method::Get, "models", None, PROBE_TIMEOUT);
        parse_models(&self.send(&request)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records requests; answers with a canned response.
    struct Fake {
        seen: Mutex<Vec<HttpRequest>>,
        answer: Result<HttpResponse, TransportError>,
    }

    impl Fake {
        fn new(status: u16, body: &str) -> Fake {
            Fake {
                seen: Mutex::new(Vec::new()),
                answer: Ok(HttpResponse {
                    status,
                    body: body.into(),
                }),
            }
        }
    }

    impl AiTransport for Fake {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.seen.lock().unwrap().push(request.clone());
            self.answer.clone()
        }
    }

    const KEY: &str = "sk-test-1234567890abcdefSECRET";

    fn key() -> Option<ApiKey> {
        ApiKey::new(KEY)
    }

    #[test]
    fn base_urls_allow_http_only_for_loopback() {
        for ok in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "HTTPS://example.com:8443/openai/v1",
            "http://localhost:11434/v1",
            "http://LOCALHOST/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:8080/v1",
            "https://[2001:db8::1]/v1",
        ] {
            assert!(BaseUrl::parse(ok).is_ok(), "{ok}: {:?}", BaseUrl::parse(ok));
        }
        assert_eq!(
            validate_base_url(" https://api.openai.com/v1/ ").unwrap(),
            "https://api.openai.com/v1"
        );
        for (bad, why) in [
            ("", "base URL"),
            ("api.openai.com/v1", "https://"),
            ("ftp://example.com", "https://"),
            ("http://example.com/v1", "localhost"),
            ("http://192.168.1.10:11434/v1", "localhost"),
            ("http://localhost.example.com/v1", "localhost"),
            ("https://user:pass@example.com/v1", "API key"),
            ("https://example.com/v1?key=1", "query"),
            ("https://exa mple.com", "spaces"),
            ("https:///v1", "host"),
        ] {
            let error = BaseUrl::parse(bad).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
        let local = BaseUrl::parse("http://127.0.0.1:1234/v1/").unwrap();
        assert!(local.is_local() && !local.is_https());
        assert_eq!(
            local.endpoint("chat/completions"),
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        assert!(BaseUrl::parse("http://[::1]/v1").unwrap().is_local());
        assert!(!BaseUrl::parse("https://api.openai.com/v1")
            .unwrap()
            .is_local());
    }

    #[test]
    fn chat_requests_carry_model_messages_and_bearer() {
        let fake = Fake::new(
            200,
            r#"{"choices":[{"message":{"role":"assistant","content":"  Budget review  "}}]}"#,
        );
        let client = AiClient::new(&fake, "https://api.example.com/v1/", key()).unwrap();
        let reply = client
            .chat(
                "gpt-test",
                &[ChatMessage::system("be brief"), ChatMessage::user("hi")],
                CHAT_TIMEOUT,
            )
            .unwrap();
        assert_eq!(reply, "Budget review");
        let seen = fake.seen.lock().unwrap();
        let request = &seen[0];
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.example.com/v1/chat/completions");
        assert_eq!(request.host, "api.example.com");
        assert!(request.https_only);
        assert_eq!(request.bearer.as_ref().unwrap().expose(), KEY);
        let body: serde_json::Value =
            serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be brief");
        assert_eq!(body["messages"][1]["role"], "user");
        assert!(body.get("temperature").is_none());
        // The key never shows up in a Debug print of the request.
        assert!(!format!("{request:?}").contains(KEY));
    }

    #[test]
    fn local_servers_without_a_key_send_no_authorization() {
        let fake = Fake::new(
            200,
            r#"{"data":[{"id":"llama3.1"},{"id":"Qwen2"},{"id":"llama3.1"}]}"#,
        );
        let client = AiClient::new(&fake, "http://localhost:11434/v1", None).unwrap();
        assert_eq!(client.list_models().unwrap(), ["llama3.1", "Qwen2"]);
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen[0].method, Method::Get);
        assert_eq!(seen[0].url, "http://localhost:11434/v1/models");
        assert!(seen[0].bearer.is_none());
        assert!(!seen[0].https_only);
        assert!(seen[0].body.is_none());
    }

    #[test]
    fn replies_parse_strings_parts_and_strip_thinking() {
        assert_eq!(
            parse_chat_reply(
                r#"{"choices":[{"message":{"content":"<think>hmm</think>\n\nTitle"}}]}"#
            )
            .unwrap(),
            "Title"
        );
        assert_eq!(
            parse_chat_reply(
                r#"{"choices":[{"message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}]}"#
            )
            .unwrap(),
            "ab"
        );
        assert_eq!(parse_chat_reply(r#"{"choices":[]}"#), Err(AiError::Empty));
        assert_eq!(
            parse_chat_reply(r#"{"choices":[{"message":{"content":null}}]}"#),
            Err(AiError::Empty)
        );
        assert!(matches!(
            parse_chat_reply("<html>proxy login</html>"),
            Err(AiError::BadResponse(_))
        ));
        assert!(matches!(parse_models("{}"), Err(AiError::BadResponse(_))));
    }

    #[test]
    fn statuses_map_to_clear_errors_without_the_key() {
        let k = key();
        let openai_401 = format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {KEY}. You can find your API key at https://platform.openai.com.","type":"invalid_request_error"}}}}"#
        );
        for (status, body, expected) in [
            (401, openai_401.as_str(), AiError::Unauthorized(401)),
            (403, "", AiError::Unauthorized(403)),
            (404, "", AiError::NotFound),
            (429, "", AiError::RateLimited),
            (500, "", AiError::Server(500)),
            (503, "<html>", AiError::Server(503)),
            (301, "", AiError::Redirected(301)),
        ] {
            let error = map_status(status, body, k.as_ref());
            assert_eq!(error, expected);
            assert!(!error.to_string().contains(KEY));
        }
        // A 400 keeps the server's explanation, scrubbed of key-like text.
        let error = map_status(
            400,
            &format!(
                r#"{{"error":{{"message":"model 'x' not allowed for key {KEY} or sk-other-ABCDEFGHIJKLMNOP"}}}}"#
            ),
            k.as_ref(),
        );
        let text = error.to_string();
        assert!(
            text.contains("(400)") && text.contains("model 'x' not allowed"),
            "{text}"
        );
        assert!(!text.contains(KEY) && !text.contains("sk-other"), "{text}");
        // Ollama's flat error shape.
        assert_eq!(
            map_status(400, r#"{"error":"model \"nope\" not found"}"#, None),
            AiError::Rejected {
                status: 400,
                detail: Some("model \"nope\" not found".into())
            }
        );
        assert_eq!(
            map_status(422, "not json", None),
            AiError::Rejected {
                status: 422,
                detail: None
            }
        );
        // Long messages are capped.
        let long = format!(r#"{{"error":{{"message":"{}"}}}}"#, "word ".repeat(200));
        let AiError::Rejected {
            detail: Some(detail),
            ..
        } = map_status(400, &long, None)
        else {
            panic!("expected a detail");
        };
        assert!(detail.chars().count() <= ERROR_DETAIL_CHARS + 1);
    }

    #[test]
    fn client_maps_statuses_and_transport_errors() {
        let fake = Fake::new(401, r#"{"error":{"message":"bad key"}}"#);
        let client = AiClient::new(&fake, "https://api.example.com/v1", key()).unwrap();
        assert_eq!(
            client.chat("m", &[ChatMessage::user("x")], CHAT_TIMEOUT),
            Err(AiError::Unauthorized(401))
        );
        let timeout = Fake {
            seen: Mutex::new(Vec::new()),
            answer: Err(TransportError::Timeout),
        };
        let client = AiClient::new(&timeout, "https://api.example.com/v1", key()).unwrap();
        assert_eq!(client.list_models(), Err(AiError::Timeout));
        let network = Fake {
            seen: Mutex::new(Vec::new()),
            answer: Err(TransportError::Network(format!("Dns Failed: {KEY}"))),
        };
        let client = AiClient::new(&network, "https://api.example.com/v1", key()).unwrap();
        let error = client.list_models().unwrap_err().to_string();
        assert!(
            error.starts_with("Could not reach") && !error.contains(KEY),
            "{error}"
        );
        // Invalid configuration never reaches the transport.
        assert!(matches!(
            AiClient::new(&network, "http://example.com", None).err(),
            Some(AiError::InvalidConfig(_))
        ));
        let client = AiClient::new(&network, "https://api.example.com/v1", None).unwrap();
        assert!(matches!(
            client.chat(" ", &[], CHAT_TIMEOUT),
            Err(AiError::InvalidConfig(_))
        ));
        assert_eq!(
            network.seen.lock().unwrap().len(),
            1,
            "only list_models above"
        );
    }

    #[test]
    fn wipe_truncate_and_model_list_caps() {
        let mut secret = String::from("sk-secret");
        let ptr = secret.as_ptr();
        wipe(&mut secret);
        assert!(secret.is_empty());
        // The same allocation now holds zeros where the secret was.
        // SAFETY: capacity ≥ 9, and the bytes were written by `wipe`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, 9) };
        assert!(bytes.iter().all(|b| *b == 0), "{bytes:?}");

        let mut text = String::from("héllo");
        truncate_bytes(&mut text, 2);
        assert_eq!(text, "h", "never splits a character");
        let mut short = String::from("ok");
        truncate_bytes(&mut short, 10);
        assert_eq!(short, "ok");

        let many: Vec<serde_json::Value> = (0..1500)
            .map(|i| serde_json::json!({ "id": format!("model-{i:04}") }))
            .chain([
                serde_json::json!({ "id": "x".repeat(300) }),
                serde_json::json!({ "id": "bad\u{0007}id" }),
            ])
            .collect();
        let body = serde_json::json!({ "data": many }).to_string();
        let models = parse_models(&body).unwrap();
        assert_eq!(models.len(), MAX_MODELS);
        assert!(models
            .iter()
            .all(|m| m.len() <= 256 && !m.contains('\u{7}')));
    }

    #[test]
    fn api_keys_are_trimmed_and_redacted() {
        assert!(ApiKey::new("   ").is_none());
        let key = ApiKey::new("  sk-abc  ").unwrap();
        assert_eq!(key.expose(), "sk-abc");
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert_eq!(format!("{:?}", Some(key)), "Some(ApiKey(<redacted>))");
    }

    /// Talks to a real OpenAI-compatible server: set WITNESS_AI_LIVE_BASE
    /// (and WITNESS_AI_LIVE_MODEL, optionally WITNESS_AI_LIVE_KEY).
    #[test]
    #[ignore = "network: needs a real OpenAI-compatible server"]
    fn live_chat_round_trip() {
        let base = std::env::var("WITNESS_AI_LIVE_BASE").expect("WITNESS_AI_LIVE_BASE");
        let model = std::env::var("WITNESS_AI_LIVE_MODEL").expect("WITNESS_AI_LIVE_MODEL");
        let key = std::env::var("WITNESS_AI_LIVE_KEY")
            .ok()
            .and_then(|k| ApiKey::new(&k));
        let client = AiClient::new(&UreqTransport, &base, key).unwrap();
        let models = client.list_models().unwrap();
        println!("{} models", models.len());
        let reply = client
            .chat(
                &model,
                &[ChatMessage::user("Reply with OK.")],
                PROBE_TIMEOUT,
            )
            .unwrap();
        println!("reply: {reply}");
    }
}
