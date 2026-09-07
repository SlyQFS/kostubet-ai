//! Telegram-слой: общее состояние, очередь к LLM, лимиты.

pub mod handlers_admin;
pub mod handlers_chat;
pub mod router;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as TokioMutex, Semaphore};

use crate::config::Config;
use crate::knowledge::matcher::Matcher;
use crate::llm::client::LlmClient;
use crate::llm::state::ModelState;
use crate::storage::SharedStore;

/// Общее состояние приложения.
pub struct App {
    pub cfg: Arc<Config>,
    pub store: SharedStore,
    pub llm: LlmClient,
    pub models: ModelState,
    pub matcher: Arc<Matcher>,
    /// Очередь параллельных запросов к LLM.
    pub queue: Arc<Semaphore>,
    /// Пейсер: не чаще одного запроса в 50 мс.
    pub last_request: Arc<TokioMutex<Instant>>,
    pub limiter: RateLimiter,
    pub bot_id: u64,
    pub bot_username: String,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("models", &self.models.list())
            .field("bot_username", &self.bot_username)
            .finish_non_exhaustive()
    }
}

impl App {
    /// Выдерживает паузу между запросами к LLM (50 мс) и фиксирует момент.
    pub async fn wait_pace(&self) {
        loop {
            let behind = {
                let mut last = self.last_request.lock().await;
                let elapsed = last.elapsed();
                if elapsed >= Duration::from_millis(50) {
                    *last = Instant::now();
                    None
                } else {
                    Some(Duration::from_millis(50) - elapsed)
                }
            };
            match behind {
                None => break,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

/// Per-user rate limiter: окно мгновений вызовов.
#[derive(Debug)]
pub struct RateLimiter {
    hits: TokioMutex<HashMap<u64, Vec<Instant>>>,
    limit: usize,
    window: Duration,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            hits: TokioMutex::new(HashMap::new()),
            limit,
            window,
        }
    }

    /// Возвращает `Some(остаток окна)`, если лимит превышен (запись не делается),
    /// иначе фиксирует вызов и возвращает None.
    pub async fn check_and_record(&self, user_id: u64) -> Option<Duration> {
        let mut hits = self.hits.lock().await;
        let now = Instant::now();
        let entry = hits.entry(user_id).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);

        if entry.len() >= self.limit {
            let oldest = entry.first().copied().unwrap_or(now);
            return Some(self.window.saturating_sub(now.duration_since(oldest)));
        }
        entry.push(now);

        // Периодическая чистка карты, чтобы не росла бесконечно.
        if hits.len() > 1000 {
            hits.retain(|_, v| v.iter().any(|t| now.duration_since(*t) < self.window));
        }
        None
    }
}
