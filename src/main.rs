mod app;
mod config;
mod media;
mod queue;
mod stats;
mod stt;
mod telegram;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::app::{admin_commands, public_commands, App, JobQueue};
use crate::config::Config;
use crate::stats::Stats;
use crate::stt::Transcriber;
use crate::telegram::Telegram;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,reqwest=warn,hyper=warn".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("create {}", cfg.data_dir.display()))?;
    // Leftovers from a crash.
    let _ = std::fs::remove_dir_all(cfg.data_dir.join("tmp"));

    let stats = Stats::open(&cfg.data_dir.join("stats.db"))?;
    let stt = Transcriber::new(cfg.stt.clone())?;
    let tg = Arc::new(Telegram::new(&cfg.api_url, &cfg.bot_token)?);

    let shutdown = CancellationToken::new();
    {
        let s = shutdown.clone();
        tokio::spawn(async move {
            wait_for_signal().await;
            tracing::info!("shutdown signal received");
            s.cancel();
        });
    }

    // The local Bot API server may still be starting; retry like the Go bot.
    let me = loop {
        match tg.get_me().await {
            Ok(me) => break me,
            Err(e) => {
                tracing::warn!("getMe failed: {e:#}; retrying in 5s");
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    };
    let bot_username = me.username.clone().unwrap_or_default();
    tracing::info!(
        "running as @{bot_username}; bot api {}; stt {}; admin {:?}; allowed {} user(s){}",
        cfg.api_url,
        stt.label(),
        cfg.admin_chat_id,
        cfg.allowed_users.len(),
        if cfg.allow_everyone {
            " + everyone"
        } else {
            ""
        }
    );
    if cfg.admin_chat_id.is_none() && cfg.allowed_users.is_empty() && !cfg.allow_everyone {
        tracing::warn!(
            "no ADMIN_CHAT_ID, ALLOWED_USER_IDS or ALLOW_EVERYONE set: nobody can use the bot"
        );
    }

    if let Err(e) = tg.set_my_commands(&public_commands(), None).await {
        tracing::warn!("setMyCommands: {e:#}");
    }
    if let Some(admin) = cfg.admin_chat_id {
        if let Err(e) = tg.set_my_commands(&admin_commands(), Some(admin)).await {
            tracing::warn!("setMyCommands (admin): {e:#}");
        }
    }

    let queue: Arc<JobQueue> = Arc::new(JobQueue::new(tg.clone(), shutdown.clone()));
    let app = Arc::new(App::new(cfg, tg, stt, stats, queue.clone(), bot_username));

    let worker_app = app.clone();
    let queue_task = tokio::spawn(async move {
        queue
            .run(move |entry| worker_app.clone().process(entry))
            .await;
    });

    app::poll(app, shutdown.clone()).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), queue_task).await;
    tracing::info!("bye");
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
