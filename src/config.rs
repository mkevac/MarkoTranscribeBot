//! Configuration from environment variables (and an optional .env file).

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::stt::{Provider, SttConfig};
use crate::telegram::CLOUD_API_URL;

#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    pub api_url: String,
    /// Admin's private chat ID (equal to their user ID). Always allowed.
    pub admin_chat_id: Option<i64>,
    pub allowed_users: HashSet<i64>,
    pub allow_everyone: bool,
    pub data_dir: PathBuf,
    pub default_language: Option<String>,
    pub chunk_seconds: u64,
    pub ffmpeg_timeout: Duration,
    pub max_media_bytes: u64,
    /// Also transcribe your own outgoing messages in business chats.
    pub business_include_own: bool,
    pub stt: SttConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let bot_token =
            get("TELEGRAM_BOT_API_TOKEN").context("TELEGRAM_BOT_API_TOKEN is required")?;

        let admin_chat_id = match get("ADMIN_CHAT_ID") {
            Some(v) => Some(
                v.parse()
                    .with_context(|| format!("invalid ADMIN_CHAT_ID {v:?}"))?,
            ),
            None => None,
        };

        let allowed_users = parse_ids(get("ALLOWED_USER_IDS").as_deref().unwrap_or(""))?;

        let provider = Provider::parse(get("STT_PROVIDER").as_deref().unwrap_or("openai"))?;
        let (default_base, default_model) = match provider {
            Provider::OpenAi => ("https://api.openai.com/v1", "gpt-transcribe"),
            Provider::ElevenLabs => ("https://api.elevenlabs.io", "scribe_v2"),
        };

        let stt = SttConfig {
            provider,
            base_url: get("STT_BASE_URL").unwrap_or_else(|| default_base.into()),
            api_key: get("STT_API_KEY"),
            model: get("STT_MODEL").unwrap_or_else(|| default_model.into()),
            prompt: get("STT_PROMPT"),
            keyterms: get("STT_KEYTERMS")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            timeout: Duration::from_secs(parse_u64(get("STT_TIMEOUT_SECONDS"), 300)?),
        };
        if provider == Provider::ElevenLabs && stt.api_key.is_none() {
            bail!("STT_API_KEY is required for ElevenLabs");
        }

        let default_language = get("STT_LANGUAGE").filter(|l| l != "auto");
        if let Some(l) = &default_language {
            if !valid_language(l) {
                bail!("STT_LANGUAGE must be an ISO-639 code like en, ru, sr or 'auto'");
            }
        }

        Ok(Self {
            bot_token,
            api_url: get("BOT_API_URL").unwrap_or_else(|| CLOUD_API_URL.into()),
            admin_chat_id,
            allowed_users,
            allow_everyone: get("ALLOW_EVERYONE").as_deref() == Some("true"),
            data_dir: PathBuf::from(get("DATA_DIR").unwrap_or_else(|| "./data".into())),
            default_language,
            chunk_seconds: parse_u64(get("CHUNK_SECONDS"), 600)?,
            ffmpeg_timeout: Duration::from_secs(parse_u64(get("FFMPEG_TIMEOUT_SECONDS"), 300)?),
            max_media_bytes: parse_size(get("MAX_MEDIA_FILESIZE").as_deref().unwrap_or("2G"))?,
            business_include_own: get("BUSINESS_INCLUDE_OWN").as_deref() == Some("true"),
            stt,
        })
    }

    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_chat_id == Some(user_id)
    }

    /// Transcription costs money, so the bot is private by default.
    pub fn is_allowed(&self, user_id: i64) -> bool {
        self.allow_everyone || self.is_admin(user_id) || self.allowed_users.contains(&user_id)
    }
}

pub fn valid_language(l: &str) -> bool {
    (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_lowercase())
}

fn parse_ids(s: &str) -> Result<HashSet<i64>> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse::<i64>()
                .with_context(|| format!("invalid user ID {p:?} in ALLOWED_USER_IDS"))
        })
        .collect()
}

fn parse_u64(v: Option<String>, default: u64) -> Result<u64> {
    match v {
        Some(v) => v.parse().with_context(|| format!("invalid number {v:?}")),
        None => Ok(default),
    }
}

/// Parses "400M", "2G", "512K" or plain bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size {s:?}"))?;
    Ok(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_ids_languages() {
        assert_eq!(parse_size("400M").unwrap(), 400 << 20);
        assert_eq!(parse_size("2g").unwrap(), 2 << 30);
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert!(parse_size("lots").is_err());

        let ids = parse_ids(" 1, 2 ,,3").unwrap();
        assert_eq!(ids.len(), 3);
        assert!(parse_ids("1,x").is_err());

        assert!(valid_language("sr"));
        assert!(valid_language("srp"));
        assert!(!valid_language("RU"));
        assert!(!valid_language("serbian"));
    }
}
