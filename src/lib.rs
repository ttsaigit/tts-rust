//! Official Rust SDK for the [TTS.ai](https://tts.ai) REST API.
//!
//! Async-first, mirrors the existing Python (`ttsai`), JS (`@ttsainpm/ttsai`),
//! PHP (`ttsai/ttsai`), Go (`github.com/ttsaigit/tts-go`), and Ruby (`ttsai`)
//! SDKs so cross-language porting is mechanical.
//!
//! ```no_run
//! use ttsai::{Client, GenerateOptions};
//!
//! # async fn run() -> Result<(), ttsai::Error> {
//! let tts = Client::new("sk-tts-...");
//! let audio = tts.generate(GenerateOptions {
//!     text: "Hello world.".into(),
//!     voice: Some("af_bella".into()),
//!     model: Some("kokoro".into()),
//!     ..Default::default()
//! }).await?;
//! tokio::fs::write("out.mp3", audio).await.unwrap();
//! # Ok(()) }
//! ```

#![deny(rust_2018_idioms)]

use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::sleep;

const DEFAULT_BASE_URL: &str = "https://api.tts.ai";

/// Synchronous-feel async client for the TTS.ai REST API.
///
/// `Client` is cheap to clone — internally uses `Arc<reqwest::Client>`.
#[derive(Debug, Clone)]
pub struct Client {
    inner: reqwest::Client,
    api_key: String,
    base_url: String,
    user_agent: String,
    max_retries: u32,
    poll_interval: Duration,
    poll_deadline: Duration,
}

/// Builder-style options for [`Client::generate`] and [`Client::generate_async`].
#[derive(Debug, Default, Clone)]
pub struct GenerateOptions {
    pub text: String,
    pub voice: Option<String>,
    pub model: Option<String>,
    pub format: Option<String>,
    pub language: Option<String>,
    pub speed: Option<f64>,
    pub instructions: Option<String>,
    /// Optional inline pronunciation overrides as `word -> replacement`.
    pub pronunciations: Option<std::collections::HashMap<String, String>>,
    /// Free-form passthrough for backend-specific knobs (cfg_weight,
    /// exaggeration, voice_design, etc.).
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Builder-style options for [`Client::clone_voice`].
#[derive(Debug, Default, Clone)]
pub struct CloneVoiceOptions {
    pub audio_path: std::path::PathBuf,
    pub text: String,
    pub model: Option<String>,
    pub language: Option<String>,
    /// Similarity slider (0.0 = natural, 1.0 = identical to reference).
    /// Honored by Chatterbox / Chatterbox-Turbo today; other models ignore.
    pub boost: Option<f64>,
}

/// Queued-state response from `/v1/tts/`, `/v1/transcribe/`, `/v1/voice-clone/`.
#[derive(Debug, Deserialize)]
pub struct Job {
    pub uuid: String,
    #[serde(default)]
    pub job_id: Option<String>,
    pub status: String,
    #[serde(default)]
    pub credits_used: Option<u64>,
    #[serde(default)]
    pub credits_remaining: Option<u64>,
    /// Server-side cache hit — if present, the audio is already at
    /// `result_url` and you don't need to poll.
    #[serde(default)]
    pub result_url: Option<String>,
    #[serde(default)]
    pub instructions_supported: Option<bool>,
    #[serde(default)]
    pub instructions_applied: Option<bool>,
    #[serde(default)]
    pub boost_supported: Option<bool>,
    #[serde(default)]
    pub boost_applied: Option<f64>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// Terminal-state response from `/v1/speech/results/`.
#[derive(Debug, Deserialize)]
pub struct JobResult {
    pub uuid: String,
    pub status: String,
    #[serde(default)]
    pub result_url: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// `/v1/speech/subtitles/` response.
#[derive(Debug, Deserialize)]
pub struct SubtitleResult {
    pub format: String,
    pub content: String,
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub segment_count: Option<u32>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
}

/// SDK error type. Use [`Error::is_*`] helpers or pattern-match on the variant
/// to handle specific cases.
#[derive(Debug, Error)]
pub enum Error {
    /// Server-provided structured error for non-2xx responses.
    #[error("ttsai: HTTP {status} ({code:?}): {message}")]
    Api {
        status: u16,
        code: Option<String>,
        message: String,
        body: Option<serde_json::Value>,
    },
    /// Job poll exceeded its deadline.
    #[error("ttsai: timeout: {0}")]
    Timeout(String),
    /// Job ended with `status=failed`.
    #[error("ttsai: job failed: {0}")]
    JobFailed(String),
    /// Network / TLS / connection error.
    #[error("ttsai: transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// JSON serialise/deserialise.
    #[error("ttsai: json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Filesystem error reading reference audio.
    #[error("ttsai: io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn is_authentication(&self) -> bool {
        matches!(self, Error::Api { status: 401, .. })
    }
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, Error::Api { status: 429, .. })
    }
    pub fn is_insufficient_credits(&self) -> bool {
        matches!(self, Error::Api { status: 402, .. })
    }
    pub fn is_validation(&self) -> bool {
        matches!(self, Error::Api { status: 400, .. })
    }
    pub fn is_server(&self) -> bool {
        matches!(self, Error::Api { status, .. } if *status >= 500)
    }
    /// Pull the `credits_needed` field from a 402 response, if present.
    pub fn credits_needed(&self) -> Option<u64> {
        self.body_field("credits_needed")
    }
    /// Pull the `credits_remaining` field from a 402 response, if present.
    pub fn credits_remaining(&self) -> Option<u64> {
        self.body_field("credits_remaining")
    }
    fn body_field(&self, name: &str) -> Option<u64> {
        if let Error::Api { body: Some(b), .. } = self {
            return b.get(name).and_then(|v| v.as_u64());
        }
        None
    }
}

impl Client {
    /// Construct a client with default settings (api.tts.ai, 180s timeout).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::builder(api_key).build()
    }

    /// Builder for non-default settings.
    pub fn builder(api_key: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            user_agent: format!("tts-rust/{} (+https://tts.ai)", env!("CARGO_PKG_VERSION")),
            max_retries: 2,
            request_timeout: Duration::from_secs(180),
            poll_interval: Duration::from_secs(1),
            poll_deadline: Duration::from_secs(600),
        }
    }

    /// Submit a TTS request, poll until the audio is ready, return the bytes.
    pub async fn generate(&self, opts: GenerateOptions) -> Result<Vec<u8>, Error> {
        let job = self.generate_async(opts).await?;
        // Server-side cache hit — audio already at result_url.
        if job.status == "completed" {
            if let Some(url) = job.result_url.as_deref() {
                return self.fetch_url(url).await;
            }
        }
        let result = self.poll_result(&job.uuid).await?;
        let url = result
            .result_url
            .ok_or_else(|| Error::JobFailed("completed without result_url".into()))?;
        self.fetch_url(&url).await
    }

    /// Submit a TTS request and return the queued [`Job`].
    pub async fn generate_async(&self, opts: GenerateOptions) -> Result<Job, Error> {
        let mut body = serde_json::Map::new();
        body.insert("text".into(), serde_json::Value::String(opts.text));
        body.insert(
            "voice".into(),
            serde_json::Value::String(opts.voice.unwrap_or_else(|| "af_bella".into())),
        );
        body.insert(
            "model".into(),
            serde_json::Value::String(opts.model.unwrap_or_else(|| "kokoro".into())),
        );
        if let Some(fmt) = opts.format {
            body.insert("format".into(), serde_json::Value::String(fmt));
        }
        if let Some(lang) = opts.language {
            body.insert("language".into(), serde_json::Value::String(lang));
        }
        if let Some(speed) = opts.speed {
            body.insert(
                "speed".into(),
                serde_json::Number::from_f64(speed)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(ins) = opts.instructions {
            body.insert("instructions".into(), serde_json::Value::String(ins));
        }
        if let Some(pron) = opts.pronunciations {
            if !pron.is_empty() {
                let m: serde_json::Map<String, serde_json::Value> = pron
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::String(v)))
                    .collect();
                body.insert("pronunciations".into(), serde_json::Value::Object(m));
            }
        }
        for (k, v) in opts.extra {
            body.insert(k, v);
        }
        self.json_request(reqwest::Method::POST, "/v1/tts/", Some(&serde_json::Value::Object(body)))
            .await
    }

    /// Block until the job reaches a terminal state.
    pub async fn poll_result(&self, uuid: &str) -> Result<JobResult, Error> {
        let deadline = Instant::now() + self.poll_deadline;
        loop {
            let path = format!(
                "/v1/speech/results/?uuid={}",
                url::form_urlencoded::byte_serialize(uuid.as_bytes()).collect::<String>()
            );
            let result: JobResult = self
                .json_request(reqwest::Method::GET, &path, None)
                .await?;
            match result.status.as_str() {
                "completed" => return Ok(result),
                "failed" => {
                    let msg = result
                        .message
                        .clone()
                        .or(result.error.clone())
                        .unwrap_or_else(|| "unknown error".into());
                    return Err(Error::JobFailed(format!("job {}: {}", uuid, msg)));
                }
                _ => {
                    if Instant::now() > deadline {
                        return Err(Error::Timeout(format!(
                            "job {} did not complete within {:?}",
                            uuid, self.poll_deadline
                        )));
                    }
                    sleep(self.poll_interval).await;
                }
            }
        }
    }

    /// Submit an audio file for transcription. Returns a queued [`Job`].
    pub async fn transcribe(&self, audio_path: impl AsRef<Path>, model: Option<&str>) -> Result<Job, Error> {
        let path = audio_path.as_ref();
        let bytes = tokio::fs::read(path).await?;
        let filename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "audio".into());
        let mut form = Form::new().text("model", model.unwrap_or("faster-whisper").to_string());
        let part = Part::bytes(bytes).file_name(filename);
        form = form.part("file", part);
        self.multipart_request("/v1/transcribe/", form).await
    }

    /// Submit a voice-clone request. Returns a queued [`Job`].
    pub async fn clone_voice(&self, opts: CloneVoiceOptions) -> Result<Job, Error> {
        let bytes = tokio::fs::read(&opts.audio_path).await?;
        let filename = opts
            .audio_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "reference.wav".into());

        let mut form = Form::new()
            .text("text", opts.text)
            .text("model", opts.model.unwrap_or_else(|| "chatterbox".into()));
        if let Some(lang) = opts.language {
            form = form.text("language", lang);
        }
        if let Some(boost) = opts.boost {
            form = form.text("boost", boost.to_string());
        }
        let part = Part::bytes(bytes).file_name(filename);
        form = form.part("reference_audio", part);
        self.multipart_request("/v1/voice-clone/", form).await
    }

    /// Voice catalog. `model` and `language` are optional filters.
    pub async fn list_voices(
        &self,
        model: Option<&str>,
        language: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let mut path = String::from("/v1/voices/");
        let mut sep = '?';
        if let Some(m) = model {
            path.push(sep);
            path.push_str("model=");
            path.push_str(&url::form_urlencoded::byte_serialize(m.as_bytes()).collect::<String>());
            sep = '&';
        }
        if let Some(l) = language {
            path.push(sep);
            path.push_str("language=");
            path.push_str(&url::form_urlencoded::byte_serialize(l.as_bytes()).collect::<String>());
        }
        let resp: serde_json::Value = self.json_request(reqwest::Method::GET, &path, None).await?;
        Ok(resp
            .get("voices")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Full `/v1/speech/models/` payload.
    pub async fn list_models(&self) -> Result<serde_json::Value, Error> {
        self.json_request(reqwest::Method::GET, "/v1/speech/models/", None)
            .await
    }

    /// SRT or VTT subtitles for a completed TTS job.
    pub async fn subtitles(&self, uuid: &str, format: &str) -> Result<SubtitleResult, Error> {
        let path = format!(
            "/v1/speech/subtitles/?uuid={}&format={}",
            url::form_urlencoded::byte_serialize(uuid.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(format.as_bytes()).collect::<String>()
        );
        self.json_request(reqwest::Method::GET, &path, None).await
    }

    // ------------------------------------------------------------------------

    async fn json_request<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        let mut attempt: u32 = 0;
        loop {
            let mut req = self
                .inner
                .request(method.clone(), &url)
                .headers(self.headers());
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await;
            match resp {
                Err(e) if attempt < self.max_retries => {
                    attempt += 1;
                    sleep(Duration::from_secs(1u64 << attempt)).await;
                    if !e.is_timeout() && !e.is_connect() {
                        return Err(Error::Transport(e));
                    }
                }
                Err(e) => return Err(Error::Transport(e)),
                Ok(r) if r.status().is_server_error() && attempt < self.max_retries => {
                    attempt += 1;
                    sleep(Duration::from_secs(1u64 << attempt)).await;
                }
                Ok(r) if !r.status().is_success() => return Err(map_error(r).await),
                Ok(r) => {
                    let bytes = r.bytes().await?;
                    if bytes.is_empty() {
                        return Ok(serde_json::from_str("null")?);
                    }
                    return Ok(serde_json::from_slice(&bytes)?);
                }
            }
        }
    }

    async fn multipart_request<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        form: Form,
    ) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .inner
            .post(&url)
            .headers(self.headers())
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error(resp).await);
        }
        let bytes = resp.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn fetch_url(&self, url: &str) -> Result<Vec<u8>, Error> {
        let resp = self.inner.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Api {
                status: resp.status().as_u16(),
                code: None,
                message: format!("download failed: HTTP {}", resp.status()),
                body: None,
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    fn headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key)).expect("bearer header"),
        );
        h.insert(USER_AGENT, HeaderValue::from_str(&self.user_agent).unwrap());
        h
    }
}

async fn map_error(resp: reqwest::Response) -> Error {
    let status = resp.status().as_u16();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Error::Transport(e),
    };
    let body: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
    let code = body
        .as_ref()
        .and_then(|b| b.get("error"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let message = body
        .as_ref()
        .and_then(|b| b.get("message").or_else(|| b.get("error")))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| std::str::from_utf8(&bytes).unwrap_or("(non-utf8 body)"))
        .to_string();
    Error::Api {
        status,
        code,
        message,
        body,
    }
}

/// Builder for [`Client`]. Consume with [`ClientBuilder::build`].
#[derive(Debug, Clone)]
pub struct ClientBuilder {
    api_key: String,
    base_url: String,
    user_agent: String,
    max_retries: u32,
    request_timeout: Duration,
    poll_interval: Duration,
    poll_deadline: Duration,
}

impl ClientBuilder {
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        let mut s: String = base_url.into();
        while s.ends_with('/') {
            s.pop();
        }
        self.base_url = s;
        self
    }
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }
    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }
    pub fn request_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d;
        self
    }
    pub fn poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }
    pub fn poll_deadline(mut self, d: Duration) -> Self {
        self.poll_deadline = d;
        self
    }
    pub fn build(self) -> Client {
        let inner = reqwest::Client::builder()
            .timeout(self.request_timeout)
            .build()
            .expect("reqwest client");
        Client {
            inner,
            api_key: self.api_key,
            base_url: self.base_url,
            user_agent: self.user_agent,
            max_retries: self.max_retries,
            poll_interval: self.poll_interval,
            poll_deadline: self.poll_deadline,
        }
    }
}

/// Re-export so users don't need a separate serde_json dep just to add Extra fields.
pub use serde_json::{json, Value as JsonValue};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_strips_trailing_slashes() {
        let c = Client::builder("sk")
            .base_url("https://example.com/")
            .build();
        assert_eq!(c.base_url, "https://example.com");
    }

    #[test]
    fn builder_strips_multiple_trailing_slashes() {
        let c = Client::builder("sk")
            .base_url("https://example.com///")
            .build();
        assert_eq!(c.base_url, "https://example.com");
    }

    #[test]
    fn error_helpers() {
        let e = Error::Api {
            status: 402,
            code: Some("insufficient_credits".into()),
            message: "need 5000".into(),
            body: Some(serde_json::json!({"credits_needed": 5000, "credits_remaining": 100})),
        };
        assert!(e.is_insufficient_credits());
        assert!(!e.is_authentication());
        assert_eq!(e.credits_needed(), Some(5000));
        assert_eq!(e.credits_remaining(), Some(100));

        let auth = Error::Api {
            status: 401,
            code: None,
            message: "bad key".into(),
            body: None,
        };
        assert!(auth.is_authentication());
        assert!(!auth.is_insufficient_credits());

        let server = Error::Api {
            status: 503,
            code: None,
            message: "down".into(),
            body: None,
        };
        assert!(server.is_server());
    }

    #[test]
    fn default_user_agent_contains_version() {
        let c = Client::builder("sk").build();
        assert!(c.user_agent.starts_with("tts-rust/"));
    }
}
