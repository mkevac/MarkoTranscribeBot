//! Update handlers and the transcription worker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use tokio_util::sync::CancellationToken;

use crate::config::{valid_language, Config};
use crate::media::{self, MediaRef};
use crate::queue::{Entry, Messenger, Queue, StatusTarget};
use crate::stats::{period_start, Record, Stats};
use crate::stt::Transcriber;
use crate::telegram::{
    cancel_keyboard, BotCommand, BusinessConnection, CallbackQuery, Message, Telegram, Update,
    User, CAPTION_LIMIT, MESSAGE_LIMIT,
};

/// Where the finished transcript goes.
#[derive(Debug, Clone)]
pub enum Target {
    /// Reply under the original message in the private chat with the bot.
    Private { chat_id: i64, reply_to: i64 },
    /// One reply in the chat where the bot was summoned (guest mode).
    Guest { query_id: String },
    /// Sent privately to the account owner; the other person sees nothing.
    Business { owner_chat_id: i64, header: String },
}

impl Target {
    fn mode(&self) -> &'static str {
        match self {
            Target::Private { .. } => "private",
            Target::Guest { .. } => "guest",
            Target::Business { .. } => "business",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub media: MediaRef,
    pub target: Target,
    pub language: Option<String>,
    pub user_id: i64,
    pub username: String,
}

pub type JobQueue = Queue<Job, Telegram>;

impl Messenger for Telegram {
    async fn send_status(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        text: &str,
        cancel_id: &str,
    ) -> Result<i64> {
        Ok(self
            .send_message(
                chat_id,
                text,
                reply_to,
                false,
                Some(cancel_keyboard(cancel_id)),
            )
            .await?
            .message_id)
    }
    async fn edit_status(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        cancel_id: Option<&str>,
    ) -> Result<()> {
        self.edit_message_text(chat_id, message_id, text, cancel_id.map(cancel_keyboard))
            .await
    }
    async fn delete_status(&self, chat_id: i64, message_id: i64) -> Result<()> {
        self.delete_message(chat_id, message_id).await
    }
}

#[derive(Clone)]
struct BizConn {
    owner: User,
    owner_chat_id: i64,
    enabled: bool,
}

pub struct App {
    pub cfg: Config,
    pub tg: Arc<Telegram>,
    pub stt: Transcriber,
    pub stats: Stats,
    pub queue: Arc<JobQueue>,
    pub bot_username: String,
    business: Mutex<HashMap<String, BizConn>>,
}

pub fn public_commands() -> Vec<BotCommand> {
    vec![
        BotCommand {
            command: "start",
            description: "Start the bot",
        },
        BotCommand {
            command: "help",
            description: "Show help",
        },
        BotCommand {
            command: "lang",
            description: "Set transcription language (e.g. /lang sr, /lang auto)",
        },
    ]
}

pub fn admin_commands() -> Vec<BotCommand> {
    let mut c = public_commands();
    c.push(BotCommand {
        command: "stats",
        description: "Show stats",
    });
    c.push(BotCommand {
        command: "users",
        description: "Show user count",
    });
    c
}

impl App {
    pub fn new(
        cfg: Config,
        tg: Arc<Telegram>,
        stt: Transcriber,
        stats: Stats,
        queue: Arc<JobQueue>,
        bot_username: String,
    ) -> Self {
        Self {
            cfg,
            tg,
            stt,
            stats,
            queue,
            bot_username,
            business: Mutex::new(HashMap::new()),
        }
    }

    pub async fn handle_update(self: Arc<Self>, upd: Update) {
        let res = if let Some(m) = upd.message {
            self.on_message(m).await
        } else if let Some(cq) = upd.callback_query {
            self.on_callback(cq).await
        } else if let Some(bc) = upd.business_connection {
            self.on_business_connection(bc).await
        } else if let Some(m) = upd.business_message {
            self.on_business_message(m).await
        } else if let Some(m) = upd.guest_message {
            self.on_guest_message(m).await
        } else {
            Ok(())
        };
        if let Err(e) = res {
            tracing::error!("update {}: {e:#}", upd.update_id);
        }
    }

    pub async fn notify_admin(&self, text: &str) {
        if let Some(id) = self.cfg.admin_chat_id {
            if let Err(e) = self.tg.send_message(id, text, None, false, None).await {
                tracing::warn!("notify admin: {e:#}");
            }
        }
    }

    fn register(&self, u: &User) {
        if let Err(e) = self.stats.register_user(
            u.id,
            u.username.as_deref().unwrap_or(""),
            &u.first_name,
            u.last_name.as_deref().unwrap_or(""),
        ) {
            tracing::warn!("register user: {e:#}");
        }
    }

    fn language_for(&self, user_id: i64) -> Option<String> {
        match self.stats.language(user_id) {
            Ok(Some(lang)) => lang,
            Ok(None) => self.cfg.default_language.clone(),
            Err(e) => {
                tracing::warn!("read language: {e:#}");
                self.cfg.default_language.clone()
            }
        }
    }

    // ---------- private chat ----------

    async fn on_message(&self, m: Message) -> Result<()> {
        if m.chat.kind != "private" {
            return Ok(()); // Groups are handled through guest mode.
        }
        let Some(user) = m.from.clone() else {
            return Ok(());
        };
        self.register(&user);
        let chat_id = m.chat.id;

        if let Some(text) = m.text.as_deref() {
            if text.starts_with('/') {
                return self.on_command(&user, chat_id, text).await;
            }
        }

        if !self.cfg.is_allowed(user.id) {
            tracing::info!("[{}] rejected: not allowed", user.handle());
            self.tg
                .send_message(
                    chat_id,
                    &format!(
                        "Sorry, this is a private bot. Your Telegram ID is {}.",
                        user.id
                    ),
                    None,
                    false,
                    None,
                )
                .await?;
            self.notify_admin(&format!(
                "Unauthorized message from {} (ID {}). Add it to ALLOWED_USER_IDS to allow.",
                user.handle(),
                user.id
            ))
            .await;
            return Ok(());
        }

        let Some(media) = media::extract_media(&m) else {
            self.tg
                .send_message(
                    chat_id,
                    "Send or forward me a voice message, a video circle, or an audio/video file.",
                    None,
                    false,
                    None,
                )
                .await?;
            return Ok(());
        };

        tracing::info!(
            "[{}] private {} ({}s)",
            user.handle(),
            media.kind.as_str(),
            media.duration.unwrap_or(0)
        );
        let job = Job {
            media,
            target: Target::Private {
                chat_id,
                reply_to: m.message_id,
            },
            language: self.language_for(user.id),
            user_id: user.id,
            username: user.handle(),
        };
        let status = StatusTarget {
            chat_id,
            reply_to: Some(m.message_id),
        };
        if let Err(e) = self
            .queue
            .add(user.id, user.handle(), job, Some(status))
            .await
        {
            tracing::error!("enqueue: {e:#}");
            self.tg
                .send_message(
                    chat_id,
                    "Sorry, couldn't queue your request. Please try again.",
                    None,
                    false,
                    None,
                )
                .await?;
        }
        Ok(())
    }

    async fn on_command(&self, user: &User, chat_id: i64, text: &str) -> Result<()> {
        let mut parts = text.split_whitespace();
        // "/lang@MyBot sr" -> "/lang"
        let cmd = parts
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let arg = parts.next().map(|s| s.to_ascii_lowercase());

        match cmd.as_str() {
            "/start" | "/help" => {
                self.tg
                    .send_message(chat_id, &self.help_text(), None, true, None)
                    .await?;
            }
            "/lang" => {
                if !self.cfg.is_allowed(user.id) {
                    return self.unauthorized(user, chat_id, "/lang").await;
                }
                let reply = match arg.as_deref() {
                    None => {
                        let cur = self.language_for(user.id).unwrap_or_else(|| "auto".into());
                        format!("Current language: {cur}\nUse /lang <code> (en, ru, sr, ...), /lang auto, or /lang default.")
                    }
                    Some("auto") => {
                        self.stats.set_language(user.id, Some("auto"))?;
                        "Language: auto-detect".into()
                    }
                    Some("default") => {
                        self.stats.set_language(user.id, None)?;
                        format!(
                            "Language reset to the server default ({}).",
                            self.cfg.default_language.as_deref().unwrap_or("auto")
                        )
                    }
                    Some(code) if valid_language(code) => {
                        self.stats.set_language(user.id, Some(code))?;
                        format!("Language: {code}")
                    }
                    Some(_) => "Use an ISO-639 code like en, ru or sr, or 'auto'.".into(),
                };
                self.tg
                    .send_message(chat_id, &reply, None, false, None)
                    .await?;
            }
            "/stats" => {
                if !self.cfg.is_admin(user.id) {
                    return self.unauthorized(user, chat_id, "/stats").await;
                }
                self.tg
                    .send_message(chat_id, &self.stats_text()?, None, true, None)
                    .await?;
            }
            "/users" => {
                if !self.cfg.is_admin(user.id) {
                    return self.unauthorized(user, chat_id, "/users").await;
                }
                let n = self.stats.user_count()?;
                self.tg
                    .send_message(
                        chat_id,
                        &format!("Total registered users: {n}"),
                        None,
                        false,
                        None,
                    )
                    .await?;
            }
            _ => {
                self.tg
                    .send_message(chat_id, "Unknown command. See /help.", None, false, None)
                    .await?;
            }
        }
        Ok(())
    }

    async fn unauthorized(&self, user: &User, chat_id: i64, what: &str) -> Result<()> {
        self.tg
            .send_message(
                chat_id,
                "You are not authorized to use this command",
                None,
                false,
                None,
            )
            .await?;
        self.notify_admin(&format!(
            "Unauthorized access to {what} from {} (ID {})",
            user.handle(),
            user.id
        ))
        .await;
        Ok(())
    }

    fn help_text(&self) -> String {
        let bot = &self.bot_username;
        format!(
            "<b>Marko Transcribe Bot</b>\n\n\
             1. <b>Private chat:</b> send or forward a voice message, video circle, or audio/video file. I reply with the text.\n\n\
             2. <b>Any chat (guest mode):</b> reply to a voice message or circle with <code>@{bot}</code>. I post the transcript there.\n\n\
             3. <b>Automatic (Telegram Business):</b> Settings → Telegram Business → Chatbots → add @{bot}. \
             Incoming voice messages and circles in the chats you choose are transcribed and sent here, privately.\n\n\
             <code>/lang sr</code>, <code>/lang auto</code>: set the transcription language.\n\
             <code>/stats</code>, <code>/users</code>: admin only.\n\n\
             Transcription engine: <code>{}</code>",
            html_escape(&self.stt.label())
        )
    }

    fn stats_text(&self) -> Result<String> {
        let mut out = String::from("<b>Summary</b>\n");
        for p in ["day", "week", "month", "overall"] {
            let s = self.stats.summary(period_start(p))?;
            out.push_str(&format!(
                "<b>{p}:</b> requests <code>{}</code>, errors <code>{}</code>, audio <code>{:.1} min</code>\n",
                s.requests,
                s.errors,
                s.audio_seconds / 60.0
            ));
        }
        out.push_str("\n<b>Top users (30 days)</b>\n");
        for u in self.stats.top_users(period_start("month"), 10)? {
            out.push_str(&format!(
                "{}: <code>{}</code> ({:.1} min)\n",
                html_escape(&u.name),
                u.requests,
                u.audio_seconds / 60.0
            ));
        }
        Ok(out)
    }

    async fn on_callback(&self, cq: CallbackQuery) -> Result<()> {
        let Some(id) = cq.data.as_deref().and_then(|d| d.strip_prefix("cancel:")) else {
            return Ok(());
        };
        let ok = self.queue.cancel(id, cq.from.id).await;
        self.tg
            .answer_callback_query(&cq.id, if ok { None } else { Some("Nothing to cancel") })
            .await
    }

    // ---------- business mode ----------

    async fn on_business_connection(&self, bc: BusinessConnection) -> Result<()> {
        let owner = bc.user.clone();
        let allowed = self.cfg.is_allowed(owner.id);
        tracing::info!(
            "business connection {} from {} enabled={} allowed={allowed}",
            bc.id,
            owner.handle(),
            bc.is_enabled
        );
        self.business.lock().unwrap().insert(
            bc.id.clone(),
            BizConn {
                owner: owner.clone(),
                owner_chat_id: bc.user_chat_id,
                enabled: bc.is_enabled,
            },
        );
        let text = if !allowed {
            self.notify_admin(&format!(
                "Business connection from unauthorized {} (ID {})",
                owner.handle(),
                owner.id
            ))
            .await;
            format!(
                "Sorry, this is a private bot. Your Telegram ID is {}.",
                owner.id
            )
        } else if bc.is_enabled {
            "✅ Connected. I'll transcribe incoming voice messages and circles in the chats you selected and send the text here. The other person doesn't see anything.".into()
        } else {
            "Disconnected. I won't transcribe your chats anymore.".into()
        };
        // Best effort: the owner may never have started the bot.
        if let Err(e) = self
            .tg
            .send_message(bc.user_chat_id, &text, None, false, None)
            .await
        {
            tracing::warn!("business connection notice: {e:#}");
        }
        Ok(())
    }

    async fn business_conn(&self, id: &str) -> Result<BizConn> {
        if let Some(c) = self.business.lock().unwrap().get(id).cloned() {
            return Ok(c);
        }
        let bc = self.tg.get_business_connection(id).await?;
        let c = BizConn {
            owner: bc.user,
            owner_chat_id: bc.user_chat_id,
            enabled: bc.is_enabled,
        };
        self.business
            .lock()
            .unwrap()
            .insert(id.to_string(), c.clone());
        Ok(c)
    }

    async fn on_business_message(&self, m: Message) -> Result<()> {
        let Some(conn_id) = m.business_connection_id.as_deref() else {
            return Ok(());
        };
        let Some(media) = media::extract_media(&m) else {
            return Ok(());
        };
        let conn = self.business_conn(conn_id).await?;
        if !conn.enabled || !self.cfg.is_allowed(conn.owner.id) {
            return Ok(());
        }
        let sender = m.from.clone().unwrap_or_default();
        if sender.id == conn.owner.id && !self.cfg.business_include_own {
            return Ok(()); // Your own outgoing message.
        }

        let header = format!("🎙 {} · {}", sender.handle(), media.kind.human());
        tracing::info!(
            "[{}] business {} from {}",
            conn.owner.handle(),
            media.kind.as_str(),
            sender.handle()
        );
        let job = Job {
            media,
            target: Target::Business {
                owner_chat_id: conn.owner_chat_id,
                header,
            },
            language: self.language_for(conn.owner.id),
            user_id: conn.owner.id,
            username: conn.owner.handle(),
        };
        self.queue
            .add(
                conn.owner.id,
                format!("{} (business)", conn.owner.handle()),
                job,
                None,
            )
            .await?;
        Ok(())
    }

    // ---------- guest mode ----------

    async fn on_guest_message(&self, m: Message) -> Result<()> {
        let Some(query_id) = m.guest_query_id.clone() else {
            return Ok(());
        };
        let Some(user) = m.from.clone() else {
            return Ok(());
        };
        self.register(&user);

        // Guest updates also arrive for replies to the bot's own messages.
        // Without media there is nothing to do, so stay silent.
        let Some(media) = media::extract_guest_media(&m) else {
            tracing::info!("[guest {}] no media, staying silent", user.handle());
            return Ok(());
        };
        if !self.cfg.is_allowed(user.id) {
            self.tg
                .answer_guest_text(
                    &query_id,
                    "Private bot",
                    "Sorry, this is a private transcription bot.",
                )
                .await?;
            self.notify_admin(&format!(
                "Unauthorized guest request from {} (ID {})",
                user.handle(),
                user.id
            ))
            .await;
            return Ok(());
        }

        tracing::info!(
            "[guest {}] {} in chat {}",
            user.handle(),
            media.kind.as_str(),
            m.chat.id
        );
        let job = Job {
            media,
            target: Target::Guest { query_id },
            language: self.language_for(user.id),
            user_id: user.id,
            username: user.handle(),
        };
        self.queue
            .add(user.id, format!("guest {}", user.handle()), job, None)
            .await?;
        Ok(())
    }

    // ---------- worker ----------

    pub async fn process(self: Arc<Self>, e: Arc<Entry<Job>>) {
        let job = &e.job;
        let result = tokio::select! {
            r = self.transcribe(&e) => r,
            _ = e.cancel.cancelled() => Err(anyhow!("canceled")),
        };

        if e.cancel.is_cancelled() {
            if let Some(s) = e.status {
                let _ = self.tg.delete_message(s.chat_id, s.message_id).await;
            }
            return;
        }

        let (seconds, err) = match &result {
            Ok((_, secs)) => (*secs, None),
            Err(err) => (0.0, Some(format!("{err:#}"))),
        };
        if let Err(db) = self.stats.record(&Record {
            user_id: job.user_id,
            username: &job.username,
            mode: job.target.mode(),
            kind: job.media.kind.as_str(),
            audio_seconds: seconds,
            error: err.as_deref(),
            provider: &self.stt.label(),
        }) {
            tracing::warn!("stats: {db:#}");
        }

        let delivered = match result {
            Ok((text, secs)) => {
                tracing::info!(
                    "[{}] transcribed {:.0}s of audio into {} chars",
                    e.log_tag,
                    secs,
                    text.chars().count()
                );
                self.deliver(&e, &text).await
            }
            Err(err) => {
                tracing::error!("[{}] transcription failed: {err:#}", e.log_tag);
                self.deliver_error(&e, &format!("{err:#}")).await
            }
        };
        if let Err(err) = delivered {
            tracing::error!("[{}] delivery failed: {err:#}", e.log_tag);
            if let Some(s) = e.status {
                let _ = self
                    .tg
                    .edit_message_text(
                        s.chat_id,
                        s.message_id,
                        &format!("❌ Failed to send transcript: {err}"),
                        None,
                    )
                    .await;
            }
        }
    }

    async fn set_status(&self, e: &Entry<Job>, text: &str) {
        if let Some(s) = e.status {
            if let Err(err) = self
                .tg
                .edit_message_text(s.chat_id, s.message_id, text, Some(cancel_keyboard(&e.id)))
                .await
            {
                tracing::warn!("[{}] status: {err:#}", e.log_tag);
            }
        }
    }

    /// Download -> convert -> split -> transcribe. Returns (text, seconds).
    async fn transcribe(&self, e: &Entry<Job>) -> Result<(String, f64)> {
        let job = &e.job;
        let tmp_root = self.cfg.data_dir.join("tmp");
        tokio::fs::create_dir_all(&tmp_root).await?;
        let work = tempfile::Builder::new()
            .prefix("job-")
            .tempdir_in(&tmp_root)?;

        self.set_status(e, &format!("⬇️ Downloading {}...", job.media.kind.human()))
            .await;
        let input =
            media::fetch_input(&self.tg, &job.media, work.path(), self.cfg.max_media_bytes).await?;

        self.set_status(e, "🎛 Extracting audio...").await;
        let speech = work.path().join("speech.ogg");
        media::to_speech_ogg(&input, &speech, self.cfg.ffmpeg_timeout).await?;
        let duration = media::probe_duration(&speech, self.cfg.ffmpeg_timeout).await?;

        let parts_dir = work.path().join("parts");
        tokio::fs::create_dir_all(&parts_dir).await?;
        let parts = media::split(
            &speech,
            &parts_dir,
            duration,
            self.cfg.chunk_seconds,
            self.cfg.ffmpeg_timeout,
        )
        .await?;

        let mut pieces: Vec<String> = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            let status = if parts.len() > 1 {
                format!("✍️ Transcribing part {}/{}...", i + 1, parts.len())
            } else {
                format!("✍️ Transcribing {:.0}s of audio...", duration)
            };
            self.set_status(e, &status).await;
            let context = pieces.last().map(String::as_str);
            let text = self
                .stt
                .transcribe(part, job.language.as_deref(), context)
                .await?;
            if !text.is_empty() {
                pieces.push(text);
            }
        }
        let text = pieces.join("\n\n");
        Ok((
            if text.trim().is_empty() {
                "(no speech detected)".into()
            } else {
                text
            },
            duration,
        ))
    }

    async fn deliver(&self, e: &Entry<Job>, text: &str) -> Result<()> {
        match &e.job.target {
            Target::Private { chat_id, reply_to } => {
                self.send_long(*chat_id, None, text, Some(*reply_to))
                    .await?;
                if let Some(s) = e.status {
                    let _ = self.tg.delete_message(s.chat_id, s.message_id).await;
                }
            }
            Target::Guest { query_id } => {
                self.tg
                    .answer_guest_text(query_id, "Transcript", &truncate(text, MESSAGE_LIMIT))
                    .await?;
            }
            Target::Business {
                owner_chat_id,
                header,
            } => {
                self.send_long(*owner_chat_id, Some(header), text, None)
                    .await?;
            }
        }
        Ok(())
    }

    async fn deliver_error(&self, e: &Entry<Job>, err: &str) -> Result<()> {
        let short = truncate(err, 500);
        match &e.job.target {
            Target::Private { .. } => {
                if let Some(s) = e.status {
                    self.tg
                        .edit_message_text(
                            s.chat_id,
                            s.message_id,
                            &format!("❌ Couldn't transcribe: {short}"),
                            None,
                        )
                        .await?;
                }
            }
            Target::Guest { query_id } => {
                self.tg
                    .answer_guest_text(
                        query_id,
                        "Transcription failed",
                        &format!("❌ Couldn't transcribe: {short}"),
                    )
                    .await?;
            }
            Target::Business {
                owner_chat_id,
                header,
            } => {
                self.tg
                    .send_message(
                        *owner_chat_id,
                        &format!("{header}\n\n❌ Couldn't transcribe: {short}"),
                        None,
                        false,
                        None,
                    )
                    .await?;
            }
        }
        self.notify_admin(&format!(
            "Transcription error for {}: {short}",
            e.job.username
        ))
        .await;
        Ok(())
    }

    /// One message when it fits, otherwise a .txt file with a preview caption.
    async fn send_long(
        &self,
        chat_id: i64,
        header: Option<&str>,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<()> {
        let full = match header {
            Some(h) => format!("{h}\n\n{text}"),
            None => text.to_string(),
        };
        if full.chars().count() <= MESSAGE_LIMIT {
            self.tg
                .send_message(chat_id, &full, reply_to, false, None)
                .await?;
        } else {
            let caption = truncate(&full, CAPTION_LIMIT);
            self.tg
                .send_text_document(
                    chat_id,
                    "transcript.txt",
                    text.to_string(),
                    &caption,
                    reply_to,
                )
                .await?;
        }
        Ok(())
    }
}

/// Truncates to `max` characters, adding an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Long-polls getUpdates until shutdown, handling updates one by one.
pub async fn poll(app: Arc<App>, shutdown: CancellationToken) {
    let mut offset = 0i64;
    let mut backoff = std::time::Duration::ZERO;
    loop {
        if !backoff.is_zero() {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
        }
        let batch = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = app.tg.get_updates(offset, 50) => r,
        };
        let raws = match batch {
            Ok(r) => {
                backoff = std::time::Duration::ZERO;
                r
            }
            Err(e) => {
                tracing::warn!("getUpdates: {e:#}");
                backoff = (backoff * 2).clamp(
                    std::time::Duration::from_secs(1),
                    std::time::Duration::from_secs(30),
                );
                continue;
            }
        };
        for raw in raws {
            let id = raw
                .get("update_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(offset);
            offset = offset.max(id + 1);
            match serde_json::from_value::<Update>(raw) {
                // Handled in order: a business_message must not overtake its
                // business_connection, and /lang must apply to the next file.
                // Handlers are quick; the slow work runs in the queue.
                Ok(upd) => app.clone().handle_update(upd).await,
                Err(e) => tracing::warn!("skip update {id}: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
        assert_eq!(truncate("приветик", 4).chars().count(), 4);
    }

    #[test]
    fn escape() {
        assert_eq!(html_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
