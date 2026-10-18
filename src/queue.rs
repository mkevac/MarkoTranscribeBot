//! FIFO job queue with per-entry cancellation and live "Queued at #N" status
//! messages. One job runs at a time, the same way as in MarkoDownloadBot.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const STATUS_STARTING: &str = "🔍 Starting...";

pub fn queued_at_text(position: usize) -> String {
    format!("👨‍👦‍👦 Queued at #{position}")
}

/// Status-message operations the queue needs. Implemented by the Telegram
/// client and by a recording fake in tests.
pub trait Messenger: Send + Sync + 'static {
    fn send_status(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        text: &str,
        cancel_id: &str,
    ) -> impl Future<Output = Result<i64>> + Send;
    fn edit_status(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        cancel_id: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;
    fn delete_status(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// Where to show a visible status message for this entry. Guest and business
/// jobs have none (they are "silent").
#[derive(Debug, Clone, Copy)]
pub struct StatusTarget {
    pub chat_id: i64,
    pub reply_to: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Status {
    pub chat_id: i64,
    pub message_id: i64,
}

pub struct Entry<J> {
    pub id: String,
    /// Only this user may press Cancel.
    pub owner_user_id: i64,
    pub log_tag: String,
    pub job: J,
    pub status: Option<Status>,
    pub cancel: CancellationToken,
}

struct State<J> {
    entries: VecDeque<Arc<Entry<J>>>,
    processing: bool,
}

pub struct Queue<J, M> {
    state: Mutex<State<J>>,
    notify: Notify,
    messenger: Arc<M>,
    shutdown: CancellationToken,
}

impl<J: Send + Sync + 'static, M: Messenger> Queue<J, M> {
    pub fn new(messenger: Arc<M>, shutdown: CancellationToken) -> Self {
        Self {
            state: Mutex::new(State {
                entries: VecDeque::new(),
                processing: false,
            }),
            notify: Notify::new(),
            messenger,
            shutdown,
        }
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    pub async fn add(
        &self,
        owner_user_id: i64,
        log_tag: String,
        job: J,
        status_target: Option<StatusTarget>,
    ) -> Result<Arc<Entry<J>>> {
        let id = uuid::Uuid::new_v4().to_string();
        let position = self.len();

        let status = match status_target {
            Some(t) => {
                let text = if position == 0 {
                    STATUS_STARTING.to_string()
                } else {
                    queued_at_text(position)
                };
                let message_id = self
                    .messenger
                    .send_status(t.chat_id, t.reply_to, &text, &id)
                    .await?;
                Some(Status {
                    chat_id: t.chat_id,
                    message_id,
                })
            }
            None => None,
        };

        let entry = Arc::new(Entry {
            log_tag: format!("{} {}", &id[..8], log_tag),
            id,
            owner_user_id,
            job,
            status,
            cancel: self.shutdown.child_token(),
        });

        let size = {
            let mut st = self.state.lock().unwrap();
            st.entries.push_back(entry.clone());
            st.entries.len()
        };
        tracing::info!(
            "queue: enqueued [{}] position=#{position} size={size}",
            entry.log_tag
        );
        self.notify.notify_one();
        Ok(entry)
    }

    /// Cancels a queued or running entry. Returns false if not found or not
    /// owned by `by_user`.
    pub async fn cancel(&self, id: &str, by_user: i64) -> bool {
        let (entry, followers) = {
            let mut st = self.state.lock().unwrap();
            let Some(idx) = st.entries.iter().position(|e| e.id == id) else {
                return false;
            };
            if st.entries[idx].owner_user_id != by_user {
                return false;
            }
            if idx == 0 && st.processing {
                // The worker cleans up its own status message.
                let e = st.entries[0].clone();
                drop(st);
                tracing::info!("queue: cancel running [{}]", e.log_tag);
                e.cancel.cancel();
                return true;
            }
            let e = st.entries.remove(idx).unwrap();
            (e, st.entries.iter().cloned().collect::<Vec<_>>())
        };
        tracing::info!("queue: cancel queued [{}]", entry.log_tag);
        entry.cancel.cancel();
        if let Some(s) = entry.status {
            if let Err(e) = self.messenger.delete_status(s.chat_id, s.message_id).await {
                tracing::warn!("queue: delete canceled status: {e:#}");
            }
        }
        self.bump_positions(&followers).await;
        true
    }

    /// Processes entries forever (until shutdown).
    pub async fn run<F, Fut>(&self, worker: F)
    where
        F: Fn(Arc<Entry<J>>) -> Fut,
        Fut: Future<Output = ()>,
    {
        loop {
            let next = {
                let mut st = self.state.lock().unwrap();
                let e = st.entries.front().cloned();
                if e.is_some() {
                    st.processing = true;
                }
                e
            };
            let Some(entry) = next else {
                tokio::select! {
                    _ = self.shutdown.cancelled() => return,
                    _ = self.notify.notified() => continue,
                }
            };

            if !entry.cancel.is_cancelled() {
                let start = Instant::now();
                tracing::info!("queue: starting [{}]", entry.log_tag);
                worker(entry.clone()).await;
                let outcome = if entry.cancel.is_cancelled() {
                    "canceled"
                } else {
                    "completed"
                };
                tracing::info!(
                    "queue: {outcome} [{}] in {:.1}s",
                    entry.log_tag,
                    start.elapsed().as_secs_f64()
                );
            }

            let followers = {
                let mut st = self.state.lock().unwrap();
                st.processing = false;
                if st
                    .entries
                    .front()
                    .map(|e| e.id == entry.id)
                    .unwrap_or(false)
                {
                    st.entries.pop_front();
                }
                st.entries.iter().cloned().collect::<Vec<_>>()
            };
            self.bump_positions(&followers).await;
            if self.shutdown.is_cancelled() {
                return;
            }
        }
    }

    /// Entry 0 is about to start; the rest move up one place.
    async fn bump_positions(&self, followers: &[Arc<Entry<J>>]) {
        for (i, f) in followers.iter().enumerate().skip(1) {
            if let Some(s) = f.status {
                if let Err(e) = self
                    .messenger
                    .edit_status(s.chat_id, s.message_id, &queued_at_text(i), Some(&f.id))
                    .await
                {
                    tracing::warn!("queue: bump position: {e:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Duration;

    #[derive(Default)]
    struct FakeMessenger {
        next_id: AtomicI64,
        log: Mutex<Vec<String>>,
    }

    impl Messenger for FakeMessenger {
        async fn send_status(
            &self,
            chat_id: i64,
            _reply_to: Option<i64>,
            text: &str,
            _cancel_id: &str,
        ) -> Result<i64> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
            self.log
                .lock()
                .unwrap()
                .push(format!("send {chat_id}/{id}: {text}"));
            Ok(id)
        }
        async fn edit_status(
            &self,
            chat_id: i64,
            message_id: i64,
            text: &str,
            _c: Option<&str>,
        ) -> Result<()> {
            self.log
                .lock()
                .unwrap()
                .push(format!("edit {chat_id}/{message_id}: {text}"));
            Ok(())
        }
        async fn delete_status(&self, chat_id: i64, message_id: i64) -> Result<()> {
            self.log
                .lock()
                .unwrap()
                .push(format!("delete {chat_id}/{message_id}"));
            Ok(())
        }
    }

    fn target(chat_id: i64) -> Option<StatusTarget> {
        Some(StatusTarget {
            chat_id,
            reply_to: None,
        })
    }

    #[tokio::test]
    async fn queued_positions_cancel_and_order() {
        let m = Arc::new(FakeMessenger::default());
        let shutdown = CancellationToken::new();
        let q: Arc<Queue<&'static str, FakeMessenger>> =
            Arc::new(Queue::new(m.clone(), shutdown.clone()));

        let gate = Arc::new(Notify::new());
        let done = Arc::new(Mutex::new(Vec::new()));

        let a = q.add(1, "a".into(), "a", target(1)).await.unwrap();
        let worker_q = q.clone();
        let (g, d) = (gate.clone(), done.clone());
        let runner = tokio::spawn(async move {
            worker_q
                .run(move |e| {
                    let (g, d) = (g.clone(), d.clone());
                    async move {
                        tokio::select! {
                            _ = g.notified() => d.lock().unwrap().push(e.job),
                            _ = e.cancel.cancelled() => d.lock().unwrap().push("canceled"),
                        }
                    }
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await; // "a" is running

        let b = q.add(2, "b".into(), "b", target(2)).await.unwrap();
        let c = q.add(3, "c".into(), "c", target(3)).await.unwrap();
        let silent = q.add(4, "d".into(), "d", None).await.unwrap();
        assert!(silent.status.is_none());

        // Wrong user cannot cancel; owner can.
        assert!(!q.cancel(&b.id, 999).await);
        assert!(q.cancel(&b.id, 2).await);

        // Cancel the running job.
        assert!(q.cancel(&a.id, 1).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.notify_one(); // c finishes
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.notify_one(); // d finishes
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(*done.lock().unwrap(), vec!["canceled", "c", "d"]);
        let log = m.log.lock().unwrap().clone();
        assert_eq!(log[0], "send 1/1: 🔍 Starting...");
        assert_eq!(log[1], "send 2/2: 👨‍👦‍👦 Queued at #1");
        assert_eq!(log[2], "send 3/3: 👨‍👦‍👦 Queued at #2");
        assert!(log.contains(&"delete 2/2".to_string()), "{log:?}");
        assert!(
            log.contains(&"edit 3/3: 👨‍👦‍👦 Queued at #1".to_string()),
            "{log:?}"
        );
        let _ = c;

        shutdown.cancel();
        runner.await.unwrap();
    }
}
