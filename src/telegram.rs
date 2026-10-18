//! Minimal Telegram Bot API client.
//!
//! We talk to the Bot API directly with reqwest + serde instead of using a bot
//! framework. Frameworks lag behind new Bot API features (guest mode, business
//! messages), and serde silently ignores fields we do not model, so this stays
//! forward compatible.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CLOUD_API_URL: &str = "https://api.telegram.org";

/// Bots can download at most 20 MB through the cloud Bot API.
pub const CLOUD_DOWNLOAD_LIMIT: u64 = 20 * 1024 * 1024;

/// Telegram's limit is 4096 UTF-16 code units; stay a little below it.
pub const MESSAGE_LIMIT: usize = 4000;
pub const CAPTION_LIMIT: usize = 1000;

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
    pub business_connection: Option<BusinessConnection>,
    pub business_message: Option<Message>,
    pub guest_message: Option<Message>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub first_name: String,
    pub last_name: Option<String>,
    pub username: Option<String>,
}

impl User {
    /// "@username" when available, otherwise the full name.
    pub fn handle(&self) -> String {
        match &self.username {
            Some(u) if !u.is_empty() => format!("@{u}"),
            _ => self.full_name(),
        }
    }

    pub fn full_name(&self) -> String {
        match &self.last_name {
            Some(l) if !l.is_empty() => format!("{} {}", self.first_name, l),
            _ => self.first_name.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type", default)]
    pub kind: String,
}

/// Shared shape of Voice, VideoNote, Audio, Video and Document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileMeta {
    pub file_id: String,
    pub file_size: Option<u64>,
    pub duration: Option<u32>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    pub voice: Option<FileMeta>,
    pub video_note: Option<FileMeta>,
    pub audio: Option<FileMeta>,
    pub video: Option<FileMeta>,
    pub document: Option<FileMeta>,
    pub reply_to_message: Option<Box<Message>>,
    pub guest_query_id: Option<String>,
    pub business_connection_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BusinessConnection {
    pub id: String,
    pub user: User,
    pub user_chat_id: i64,
    #[serde(default)]
    pub is_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgFile {
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BotCommand {
    pub command: &'static str,
    pub description: &'static str,
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<i64>,
    parameters: Option<ResponseParameters>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    api_url: String,
    token: String,
}

impl Telegram {
    pub fn new(api_url: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Long polling waits up to 50 s; leave headroom.
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    /// True when talking to a self-hosted Bot API server (no 20 MB limit).
    pub fn is_local_server(&self) -> bool {
        self.api_url != CLOUD_API_URL
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_url, self.token, method)
    }

    pub fn file_url(&self, file_path: &str) -> String {
        format!("{}/file/bot{}/{}", self.api_url, self.token, file_path)
    }

    /// Calls a Bot API method with a JSON body. Retries on flood control (429).
    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: &Value) -> Result<T> {
        for _ in 0..3 {
            let resp = self
                .http
                .post(self.method_url(method))
                .json(params)
                .send()
                .await
                // without_url(): the URL contains the bot token.
                .map_err(|e| anyhow!("{method}: {}", e.without_url()))?;
            let parsed: ApiResponse<T> = resp
                .json()
                .await
                .map_err(|e| anyhow!("{method}: decode response: {}", e.without_url()))?;
            if parsed.ok {
                return parsed
                    .result
                    .ok_or_else(|| anyhow!("{method}: ok response without result"));
            }
            if parsed.error_code == Some(429) {
                let wait = parsed.parameters.and_then(|p| p.retry_after).unwrap_or(1);
                tokio::time::sleep(Duration::from_secs(wait.min(60))).await;
                continue;
            }
            bail!(
                "{method} failed ({}): {}",
                parsed.error_code.unwrap_or_default(),
                parsed.description.unwrap_or_default()
            );
        }
        bail!("{method}: still rate limited after retries")
    }

    pub async fn get_me(&self) -> Result<User> {
        self.call("getMe", &json!({})).await
    }

    pub async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>> {
        self.call(
            "getUpdates",
            &json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": [
                    "message", "callback_query",
                    "business_connection", "business_message",
                    "guest_message",
                ],
            }),
        )
        .await
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
        html: bool,
        markup: Option<Value>,
    ) -> Result<Message> {
        let mut p = json!({
            "chat_id": chat_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
        });
        if html {
            p["parse_mode"] = json!("HTML");
        }
        if let Some(id) = reply_to {
            p["reply_parameters"] =
                json!({ "message_id": id, "allow_sending_without_reply": true });
        }
        if let Some(m) = markup {
            p["reply_markup"] = m;
        }
        self.call("sendMessage", &p).await
    }

    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        markup: Option<Value>,
    ) -> Result<()> {
        let mut p = json!({ "chat_id": chat_id, "message_id": message_id, "text": text });
        if let Some(m) = markup {
            p["reply_markup"] = m;
        }
        match self.call::<Value>("editMessageText", &p).await {
            Ok(_) => Ok(()),
            // Editing to identical text is not an error for us.
            Err(e) if e.to_string().contains("message is not modified") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        self.call::<Value>(
            "deleteMessage",
            &json!({ "chat_id": chat_id, "message_id": message_id }),
        )
        .await
        .map(|_| ())
    }

    pub async fn answer_callback_query(&self, id: &str, text: Option<&str>) -> Result<()> {
        let mut p = json!({ "callback_query_id": id });
        if let Some(t) = text {
            p["text"] = json!(t);
        }
        self.call::<Value>("answerCallbackQuery", &p)
            .await
            .map(|_| ())
    }

    pub async fn set_my_commands(
        &self,
        commands: &[BotCommand],
        chat_id: Option<i64>,
    ) -> Result<()> {
        let mut p = json!({ "commands": commands });
        if let Some(id) = chat_id {
            p["scope"] = json!({ "type": "chat", "chat_id": id });
        }
        self.call::<Value>("setMyCommands", &p).await.map(|_| ())
    }

    pub async fn get_file(&self, file_id: &str) -> Result<TgFile> {
        self.call("getFile", &json!({ "file_id": file_id })).await
    }

    pub async fn get_business_connection(&self, id: &str) -> Result<BusinessConnection> {
        self.call(
            "getBusinessConnection",
            &json!({ "business_connection_id": id }),
        )
        .await
    }

    /// Replies to a guest message (Bot API 10.0 guest mode) with a text article.
    pub async fn answer_guest_text(
        &self,
        guest_query_id: &str,
        title: &str,
        text: &str,
    ) -> Result<()> {
        let result = json!({
            "type": "article",
            "id": uuid::Uuid::new_v4().to_string(),
            "title": title,
            "input_message_content": { "message_text": text },
        });
        self.call::<Value>(
            "answerGuestQuery",
            &json!({ "guest_query_id": guest_query_id, "result": result }),
        )
        .await
        .map(|_| ())
    }

    /// Uploads an in-memory text file as a document.
    pub async fn send_text_document(
        &self,
        chat_id: i64,
        file_name: &str,
        contents: String,
        caption: &str,
        reply_to: Option<i64>,
    ) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(contents.into_bytes())
            .file_name(file_name.to_string())
            .mime_str("text/plain; charset=utf-8")?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", caption.to_string())
            .part("document", part);
        if let Some(id) = reply_to {
            form = form.text(
                "reply_parameters",
                json!({ "message_id": id, "allow_sending_without_reply": true }).to_string(),
            );
        }
        let resp = self
            .http
            .post(self.method_url("sendDocument"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow!("sendDocument: {}", e.without_url()))?;
        let parsed: ApiResponse<Value> = resp
            .json()
            .await
            .map_err(|e| anyhow!("sendDocument: decode: {}", e.without_url()))?;
        if !parsed.ok {
            bail!(
                "sendDocument failed ({}): {}",
                parsed.error_code.unwrap_or_default(),
                parsed.description.unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Streams a cloud-hosted file to `dest`.
    pub async fn download_to(&self, file_path: &str, dest: &std::path::Path) -> Result<()> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let resp = self
            .http
            .get(self.file_url(file_path))
            .send()
            .await
            .map_err(|e| anyhow!("download: {}", e.without_url()))?;
        if !resp.status().is_success() {
            bail!("download failed: HTTP {}", resp.status());
        }
        let mut out = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("create {}", dest.display()))?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow!("download: {}", e.without_url()))?;
            out.write_all(&chunk).await?;
        }
        out.flush().await?;
        Ok(())
    }
}

pub fn cancel_keyboard(id: &str) -> Value {
    json!({ "inline_keyboard": [[{ "text": "❌ Cancel", "callback_data": format!("cancel:{id}") }]] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_guest_message_with_replied_video_note() {
        let raw = r#"{
            "update_id": 10,
            "guest_message": {
                "message_id": 5, "date": 1,
                "chat": {"id": -100, "type": "supergroup", "title": "Friends"},
                "from": {"id": 42, "is_bot": false, "first_name": "Marko", "username": "marko"},
                "text": "@MarkoTranscribeBot",
                "guest_query_id": "gq-1",
                "reply_to_message": {
                    "message_id": 4, "date": 1,
                    "chat": {"id": -100, "type": "supergroup"},
                    "video_note": {"file_id": "VN", "file_unique_id": "u", "length": 384, "duration": 12, "file_size": 900000},
                    "some_future_field": {"x": 1}
                }
            }
        }"#;
        let u: Update = serde_json::from_str(raw).unwrap();
        let gm = u.guest_message.unwrap();
        assert_eq!(gm.guest_query_id.as_deref(), Some("gq-1"));
        let vn = gm.reply_to_message.unwrap().video_note.unwrap();
        assert_eq!(vn.file_id, "VN");
        assert_eq!(vn.duration, Some(12));
    }

    #[test]
    fn parses_business_message_and_connection() {
        let raw = r#"{
            "update_id": 11,
            "business_connection": {
                "id": "bc-1", "date": 1, "user_chat_id": 42, "is_enabled": true,
                "user": {"id": 42, "first_name": "Marko"},
                "rights": {"can_reply": true}
            }
        }"#;
        let u: Update = serde_json::from_str(raw).unwrap();
        let bc = u.business_connection.unwrap();
        assert_eq!(bc.user_chat_id, 42);
        assert!(bc.is_enabled);

        let raw = r#"{
            "update_id": 12,
            "business_message": {
                "message_id": 7, "date": 1, "business_connection_id": "bc-1",
                "chat": {"id": 77, "type": "private", "first_name": "Ana"},
                "from": {"id": 77, "first_name": "Ana"},
                "voice": {"file_id": "V", "file_unique_id": "u", "duration": 3, "mime_type": "audio/ogg"}
            }
        }"#;
        let u: Update = serde_json::from_str(raw).unwrap();
        let m = u.business_message.unwrap();
        assert_eq!(m.business_connection_id.as_deref(), Some("bc-1"));
        assert_eq!(m.voice.unwrap().file_id, "V");
    }

    #[test]
    fn user_handle_prefers_username() {
        let mut u = User {
            id: 1,
            first_name: "Ana".into(),
            last_name: Some("P".into()),
            ..Default::default()
        };
        assert_eq!(u.handle(), "Ana P");
        u.username = Some("ana".into());
        assert_eq!(u.handle(), "@ana");
    }
}
