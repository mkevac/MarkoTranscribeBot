//! Speech-to-text providers.
//!
//! - `openai`: any OpenAI-compatible `/audio/transcriptions` endpoint. This
//!   covers OpenAI itself, Groq, and self-hosted servers (speaches,
//!   whisper.cpp server, LocalAI, vLLM).
//! - `elevenlabs`: ElevenLabs Scribe (`/v1/speech-to-text`).

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    ElevenLabs,
}

impl Provider {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "groq" => Ok(Provider::OpenAi),
            "elevenlabs" | "scribe" => Ok(Provider::ElevenLabs),
            other => bail!("unknown STT_PROVIDER {other:?} (use openai or elevenlabs)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SttConfig {
    pub provider: Provider,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    /// Fixed hint: names, jargon, expected languages.
    pub prompt: Option<String>,
    /// ElevenLabs keyterms (billed extra by ElevenLabs).
    pub keyterms: Vec<String>,
    pub timeout: Duration,
}

impl SttConfig {
    pub fn label(&self) -> String {
        let p = match self.provider {
            Provider::OpenAi => "openai-compatible",
            Provider::ElevenLabs => "elevenlabs",
        };
        format!("{p}:{}", self.model)
    }
}

#[derive(Deserialize)]
struct TextResponse {
    text: String,
}

pub struct Transcriber {
    http: reqwest::Client,
    cfg: SttConfig,
}

impl Transcriber {
    pub fn new(cfg: SttConfig) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(cfg.timeout).build()?;
        Ok(Self { http, cfg })
    }

    pub fn label(&self) -> String {
        self.cfg.label()
    }

    /// Transcribes one audio file. `language` is an ISO-639-1 code or None
    /// for auto-detect. `context` is the tail of the previous chunk, used as
    /// a prompt so long recordings stay consistent across chunk boundaries.
    pub async fn transcribe(
        &self,
        path: &Path,
        language: Option<&str>,
        context: Option<&str>,
    ) -> Result<String> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;

        let mut delay = Duration::from_secs(2);
        let mut last_err = anyhow!("no attempts made");
        for attempt in 1..=3 {
            match self.attempt(bytes.clone(), language, context).await {
                Ok(t) => return Ok(t.trim().to_string()),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Retryable(e)) => {
                    tracing::warn!("stt attempt {attempt} failed: {e:#}");
                    last_err = e;
                    if attempt < 3 {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                }
            }
        }
        Err(last_err)
    }

    async fn attempt(
        &self,
        bytes: Vec<u8>,
        language: Option<&str>,
        context: Option<&str>,
    ) -> Result<String, AttemptError> {
        let part = Part::bytes(bytes)
            .file_name("audio.ogg")
            .mime_str("audio/ogg")
            .map_err(|e| AttemptError::Fatal(e.into()))?;

        let req = match self.cfg.provider {
            Provider::OpenAi => {
                let mut form = Form::new()
                    .part("file", part)
                    .text("model", self.cfg.model.clone())
                    .text("response_format", "json");
                if let Some(lang) = language {
                    form = form.text("language", lang.to_string());
                }
                if let Some(prompt) = build_prompt(self.cfg.prompt.as_deref(), context) {
                    form = form.text("prompt", prompt);
                }
                let url = format!(
                    "{}/audio/transcriptions",
                    self.cfg.base_url.trim_end_matches('/')
                );
                let mut req = self.http.post(url).multipart(form);
                if let Some(key) = &self.cfg.api_key {
                    req = req.bearer_auth(key);
                }
                req
            }
            Provider::ElevenLabs => {
                let mut form = Form::new()
                    .part("file", part)
                    .text("model_id", self.cfg.model.clone())
                    .text("tag_audio_events", "false");
                if let Some(lang) = language {
                    form = form.text("language_code", lang.to_string());
                }
                for k in &self.cfg.keyterms {
                    form = form.text("keyterms", k.clone());
                }
                let url = format!(
                    "{}/v1/speech-to-text",
                    self.cfg.base_url.trim_end_matches('/')
                );
                let key = self.cfg.api_key.clone().ok_or_else(|| {
                    AttemptError::Fatal(anyhow!("STT_API_KEY is required for ElevenLabs"))
                })?;
                self.http
                    .post(url)
                    .multipart(form)
                    .header("xi-api-key", key)
            }
        };

        let resp = req
            .send()
            .await
            .map_err(|e| AttemptError::Retryable(anyhow!("request failed: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AttemptError::Retryable(anyhow!("read body: {e}")))?;
        if !status.is_success() {
            let snippet: String = body.chars().take(300).collect();
            let err = anyhow!("speech-to-text API returned HTTP {status}: {snippet}");
            return Err(if status.as_u16() == 429 || status.is_server_error() {
                AttemptError::Retryable(err)
            } else {
                AttemptError::Fatal(err)
            });
        }
        let parsed: TextResponse = serde_json::from_str(&body).map_err(|e| {
            AttemptError::Fatal(anyhow!(
                "unexpected response ({e}): {}",
                body.chars().take(200).collect::<String>()
            ))
        })?;
        Ok(parsed.text)
    }
}

enum AttemptError {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

/// Joins the static prompt and the previous chunk's tail. Whisper only uses
/// the last ~224 tokens of a prompt, so keep the context short.
fn build_prompt(fixed: Option<&str>, context: Option<&str>) -> Option<String> {
    let tail = context.map(|c| {
        let chars: Vec<char> = c.chars().collect();
        let start = chars.len().saturating_sub(300);
        chars[start..].iter().collect::<String>()
    });
    let parts: Vec<String> = [fixed.map(str::to_string), tail]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn prompt_building() {
        assert_eq!(build_prompt(None, None), None);
        assert_eq!(
            build_prompt(Some(" Names: Ana "), None).as_deref(),
            Some("Names: Ana")
        );
        let long = "я".repeat(1000);
        let p = build_prompt(None, Some(&long)).unwrap();
        assert_eq!(p.chars().count(), 300);
    }

    /// One-shot HTTP server: captures the request, returns the given response.
    async fn fake_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            // Read until the multipart closing boundary arrives.
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let s = String::from_utf8_lossy(&buf);
                if s.contains("--\r\n") && s.trim_end().ends_with("--") {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf).to_string()
        });
        (format!("http://{addr}"), handle)
    }

    fn cfg(provider: Provider, base: String) -> SttConfig {
        SttConfig {
            provider,
            base_url: base,
            api_key: Some("sk-test".into()),
            model: "test-model".into(),
            prompt: Some("Names: Marko".into()),
            keyterms: vec!["Perplexity".into()],
            timeout: Duration::from_secs(10),
        }
    }

    async fn audio_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.ogg");
        tokio::fs::write(&p, b"OggS fake audio").await.unwrap();
        (dir, p)
    }

    #[tokio::test]
    async fn openai_compatible_request_shape() {
        let (base, server) = fake_server("200 OK", r#"{"text":" Привет, Марко. "}"#).await;
        let (_d, path) = audio_file().await;
        let t = Transcriber::new(cfg(Provider::OpenAi, format!("{base}/v1"))).unwrap();
        let text = t
            .transcribe(&path, Some("ru"), Some("previous chunk"))
            .await
            .unwrap();
        assert_eq!(text, "Привет, Марко.");

        let req = server.await.unwrap();
        assert!(req.starts_with("POST /v1/audio/transcriptions"), "{req}");
        assert!(req.to_lowercase().contains("authorization: bearer sk-test"));
        assert!(req.contains("name=\"model\"\r\n\r\ntest-model"));
        assert!(req.contains("name=\"language\"\r\n\r\nru"));
        assert!(req.contains("Names: Marko\nprevious chunk"));
        assert!(req.contains("filename=\"audio.ogg\""));
    }

    #[tokio::test]
    async fn elevenlabs_request_shape() {
        let (base, server) =
            fake_server("200 OK", r#"{"language_code":"sr","text":"Zdravo"}"#).await;
        let (_d, path) = audio_file().await;
        let t = Transcriber::new(cfg(Provider::ElevenLabs, base)).unwrap();
        assert_eq!(t.transcribe(&path, None, None).await.unwrap(), "Zdravo");

        let req = server.await.unwrap();
        assert!(req.starts_with("POST /v1/speech-to-text"), "{req}");
        assert!(req.to_lowercase().contains("xi-api-key: sk-test"));
        assert!(req.contains("name=\"model_id\"\r\n\r\ntest-model"));
        assert!(req.contains("name=\"keyterms\"\r\n\r\nPerplexity"));
        assert!(!req.contains("name=\"language_code\""));
    }

    #[tokio::test]
    async fn client_errors_are_not_retried() {
        let (base, server) = fake_server("401 Unauthorized", r#"{"error":"bad key"}"#).await;
        let (_d, path) = audio_file().await;
        let t = Transcriber::new(cfg(Provider::OpenAi, base)).unwrap();
        let err = t
            .transcribe(&path, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401") && err.contains("bad key"), "{err}");
        server.await.unwrap();
    }
}
