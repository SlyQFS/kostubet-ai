use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as TokioMutex, Semaphore};

use crate::config::Config;
use crate::db::MemoryStore;
use crate::llm::{ChatMessage, LlmClient, LlmSettings};
use crate::text::meaningful_words;

/// Сколько символов релевантных выдержек из гайдов максимум подмешивается в контекст.
const GUIDE_CONTEXT_CHARS: usize = 4000;

/// Ключи настроек в таблице `settings`, переопределяющие конфиг из .env.
pub const SETTING_BASE_URL: &str = "llm_base_url";
pub const SETTING_API_KEY: &str = "llm_api_key";
pub const SETTING_MODEL: &str = "llm_model";
pub const SETTING_SYSTEM_PROMPT: &str = "system_prompt";

/// Ограничитель частоты запросов: не более `max_requests` обращений к LLM
/// за `window` от каждого пользователя. Предотвращает монополизацию очереди
/// одним пользователем и гарантирует, что пока бот генерирует ответ для
/// одного собеседника, залповый спам от него не забивает очередь —
/// остальные пользователи получают ответы вовремя.
#[derive(Clone)]
pub struct RateLimiter {
    requests: Arc<TokioMutex<HashMap<i64, Vec<Instant>>>>,
    max_requests: usize,
    window: Duration,
}

impl RateLimiter {
    pub fn new(max_requests: usize, window: Duration) -> Self {
        Self {
            requests: Arc::new(TokioMutex::new(HashMap::new())),
            max_requests,
            window,
        }
    }

    /// Атомарно проверяет и регистрирует запрос. Возвращает `true`, если
    /// запрос разрешён (и записывает метку времени), `false` — если лимит исчерпан.
    pub async fn check_and_record(&self, user_id: i64) -> bool {
        let mut requests = self.requests.lock().await;
        let now = Instant::now();
        // Периодическая очистка от неактивных пользователей при разрастании карты
        if requests.len() > 1000 {
            requests.retain(|_, timestamps| {
                timestamps.retain(|t| now.saturating_duration_since(*t) < self.window);
                !timestamps.is_empty()
            });
        }
        let user_requests = requests.entry(user_id).or_default();
        user_requests.retain(|t| now.saturating_duration_since(*t) < self.window);
        if user_requests.len() >= self.max_requests {
            return false;
        }
        user_requests.push(now);
        true
    }

    /// Сколько запросов ещё доступно пользователю в текущем окне.
    #[allow(dead_code)]
    pub async fn remaining(&self, user_id: i64) -> usize {
        let mut requests = self.requests.lock().await;
        let now = Instant::now();
        let user_requests = requests.entry(user_id).or_default();
        user_requests.retain(|t| now.saturating_duration_since(*t) < self.window);
        self.max_requests.saturating_sub(user_requests.len())
    }
}

pub fn build_system_prompt(base_prompt: &str, guide_hits: &[(String, String)]) -> String {
    let mut prompt = base_prompt.trim().to_string();
    if !guide_hits.is_empty() {
        prompt.push_str(
            "\n\nВыдержки из базы знаний (гайдов), релевантные текущему вопросу. \
             Используй их, если они действительно относятся к делу; если не относятся — игнорируй:\n",
        );
        for (title, chunk) in guide_hits {
            prompt.push_str(&format!("\n— Из гайда «{title}»:\n{chunk}\n"));
        }
    }
    prompt
}

/// Память чата для команды /memory.
pub struct ChatSnapshot {
    pub message_count: i64,
    pub est_tokens: usize,
    pub participants: usize,
    pub budget: usize,
}

/// Менеджер памяти и очереди запросов.
/// Хранит полу-разделяемую дословную историю сообщений по чатам: все участники
/// группового чата видят общий контекст. Бюджет токенов масштабируется по
/// числу участников (1 → base, 6+ → max). Очередь защищена семафором
/// параллельности и блокировкой на чат (запросы из одного чата идут строго
/// последовательно, чтобы не гоняться за общую историю).
#[derive(Clone)]
pub struct MemoryManager {
    store: Arc<Mutex<MemoryStore>>,
    llm: LlmClient,
    cfg: Arc<Config>,
    semaphore: Arc<Semaphore>,
    last_request_time: Arc<TokioMutex<Instant>>,
    rate_limiter: RateLimiter,
}

impl MemoryManager {
    pub fn new(store: Arc<Mutex<MemoryStore>>, llm: LlmClient, cfg: Arc<Config>) -> Self {
        let max_concurrent = cfg.max_concurrent_requests;
        let rate_limiter = RateLimiter::new(
            cfg.rate_limit_requests,
            Duration::from_secs(cfg.rate_limit_window_secs),
        );
        Self {
            store,
            llm,
            cfg,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            last_request_time: Arc::new(TokioMutex::new(Instant::now() - Duration::from_secs(10))),
            rate_limiter,
        }
    }

    fn store(&self) -> MutexGuard<'_, MemoryStore> {
        self.store.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Прогрессия бюджета токенов по числу участников:
    /// 1 участник → base (USER_MEMORY_TOKENS), 6+ → max (MAX_GROUP_MEMORY_TOKENS).
    /// Линейная прогрессия с шагом (max - base) / 5.
    pub fn compute_chat_token_budget(&self, participants: usize) -> usize {
        let base = self.cfg.user_memory_tokens;
        let max = self.cfg.max_group_memory_tokens;
        if participants <= 1 || max <= base {
            return base;
        }
        let step = ((max - base) / 5).max(1);
        let budget = base + (participants - 1) * step;
        budget.min(max)
    }

    /// Проверяет и регистрирует запрос пользователя в рамках частотного лимита.
    /// Возвращает `true` — запрос разрешён, `false` — лимит исчерпан.
    pub async fn check_rate_limit(&self, user_id: i64) -> bool {
        self.rate_limiter.check_and_record(user_id).await
    }

    /// Сколько запросов ещё доступно пользователю в текущем окне.
    #[allow(dead_code)]
    pub async fn rate_limit_remaining(&self, user_id: i64) -> usize {
        self.rate_limiter.remaining(user_id).await
    }

    /// Возвращает настроенный лимит запросов на пользователя.
    pub fn rate_limit_max(&self) -> usize {
        self.cfg.rate_limit_requests
    }

    /// Возвращает длину окна частотного лимита в секундах.
    pub fn rate_limit_window_secs(&self) -> u64 {
        self.cfg.rate_limit_window_secs
    }

    /// Синхронная/нестриминговая обёртка над reply_stream.
    #[allow(dead_code)]
    pub async fn reply(
        &self,
        user_id: i64,
        chat_id: i64,
        display_name: &str,
        text: &str,
    ) -> Result<String, String> {
        self.reply_stream(user_id, chat_id, display_name, text, |_| {}).await
    }

    /// Стриминговая генерация ответа с полу-разделяемой памятью чата.
    /// Бюджет токенов масштабируется по числу участников; все сообщения
    /// участников чата попадают в общий контекст LLM.
    pub async fn reply_stream<F>(
        &self,
        user_id: i64,
        chat_id: i64,
        display_name: &str,
        text: &str,
        on_delta: F,
    ) -> Result<String, String>
    where
        F: FnMut(&str) + Send,
    {
        let user_content: String = if text.chars().count() > 15_000 {
            let mut cut: String = text.chars().take(15_000).collect();
            cut.push_str("\n[сообщение обрезано из-за длины]");
            cut
        } else {
            text.to_string()
        };

        // Сохраняем входящее сообщение до сетевого вызова
        {
            let store = self.store();
            store
                .add_message(user_id, chat_id, display_name, "user", &user_content)
                .map_err(|e| format!("ошибка базы данных: {e}"))?;
        }

        // Вычисляем масштабированный бюджет по числу участников чата
        let (history, guide_hits, budget) = {
            let store = self.store();
            let participants = store.chat_participant_count(chat_id).unwrap_or(1).max(1);
            let budget = self.compute_chat_token_budget(participants);
            let history = store
                .chat_messages_within_tokens(chat_id, budget)
                .map_err(|e| format!("ошибка базы данных: {e}"))?;
            let guide_hits = store
                .search_guide_chunks(&meaningful_words(&user_content), GUIDE_CONTEXT_CHARS)
                .unwrap_or_default();
            (history, guide_hits, budget)
        };

        let base_prompt = self.effective_system_prompt();
        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(ChatMessage::system(build_system_prompt(&base_prompt, &guide_hits)));
        for m in history {
            if m.role == "assistant" {
                messages.push(ChatMessage::assistant(m.content));
            } else if let Some(name) = m.display_name.filter(|n| !n.is_empty()) {
                messages.push(ChatMessage::user_named(name, m.content));
            } else {
                messages.push(ChatMessage::user(m.content));
            }
        }

        // Эффективные настройки подключения: переопределение из БД либо .env
        let settings = self.effective_settings();

        // Захват слота очереди (семафора). Если все слоты заняты — ожидаем в FIFO очереди.
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| "внутренняя ошибка очереди".to_string())?;

        // Пейсер запросов: сглаживает микро-всплески (burst)
        {
            let mut last = self.last_request_time.lock().await;
            let min_interval = Duration::from_millis(50);
            let elapsed = last.elapsed();
            if elapsed < min_interval {
                tokio::time::sleep(min_interval - elapsed).await;
            }
            *last = Instant::now();
        }

        let answer = self
            .llm
            .stream_chat(&settings, &messages, self.cfg.max_reply_tokens, self.cfg.temperature, on_delta)
            .await
            .map_err(|e| format!("{e}"))?;

        // Освобождение семафора происходит автоматически при дропе _permit
        drop(_permit);

        {
            let store = self.store();
            store
                .add_message(user_id, chat_id, "", "assistant", answer.trim())
                .map_err(|e| format!("ошибка базы данных: {e}"))?;
            match store.trim_chat_history(chat_id, budget) {
                Ok(deleted) if deleted > 0 => {
                    tracing::debug!(chat_id, deleted, "старые сообщения вышли за лимит памяти и удалены");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("не удалось обрезать историю чата {chat_id}: {e}"),
            }
        }

        self.check_global_limit();
        Ok(answer)
    }

    /// Проверяет общий размер базы; при превышении 95% лимита чистит.
    fn check_global_limit(&self) {
        let mut store = self.store();
        if store.db_disk_size() > self.cfg.memory_limit_bytes * 95 / 100 {
            tracing::info!("база памяти превысила 95% лимита — запускаю чистку");
            match store.enforce_limit(self.cfg.memory_limit_bytes) {
                Ok(stage) => tracing::info!(stage, "чистка базы завершена"),
                Err(e) => tracing::error!("не удалось очистить базу: {e}"),
            }
        }
    }

    pub fn reset_chat(&self, chat_id: i64) -> Result<(), String> {
        self.store().reset_chat(chat_id).map_err(|e| e.to_string())
    }

    pub fn chat_snapshot(&self, chat_id: i64) -> ChatSnapshot {
        let (message_count, est_tokens, participants) =
            self.store().chat_stats(chat_id).unwrap_or((0, 0, 0));
        let budget = self.compute_chat_token_budget(participants);
        ChatSnapshot {
            message_count,
            est_tokens,
            participants,
            budget,
        }
    }

    /// (размер базы на диске, всего сообщений, всего пользователей, гайдов)
    pub fn global_stats(&self) -> (u64, i64, i64, i64) {
        let store = self.store();
        (
            store.db_disk_size(),
            store.total_messages().unwrap_or(0),
            store.total_users().unwrap_or(0),
            store.guides_count().unwrap_or(0),
        )
    }

    pub fn guides(&self) -> Vec<crate::db::GuideInfo> {
        self.store().guides().unwrap_or_default()
    }

    pub fn find_guide(&self, query: &str) -> Option<i64> {
        self.store().find_guide(query).unwrap_or(None)
    }

    pub fn delete_guide(&self, guide_id: i64) -> bool {
        self.store().delete_guide(guide_id).unwrap_or(false)
    }

    pub fn add_guide(&self, title: &str, added_by: i64, content: &str) -> Result<usize, String> {
        self.store().add_guide(title, added_by, content).map_err(|e| e.to_string())
    }

    #[allow(dead_code)]
    pub fn user_memory_tokens(&self) -> usize {
        self.cfg.user_memory_tokens
    }

    // ===== Настройки LLM и системный промпт (админ-панель) =====

    /// Эффективные настройки: значение из БД, если задано, иначе дефолт из .env.
    pub fn effective_settings(&self) -> LlmSettings {
        let store = self.store();
        let base_url = store
            .get_setting(SETTING_BASE_URL)
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| self.cfg.llm_base_url.clone());
        let api_key = store
            .get_setting(SETTING_API_KEY)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.cfg.llm_api_key.clone());
        let model = store
            .get_setting(SETTING_MODEL)
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| self.cfg.llm_model.clone());
        LlmSettings { base_url, api_key, model }
    }

    /// Эффективный системный промпт (из БД, если переопределён, иначе дефолт).
    pub fn effective_system_prompt(&self) -> String {
        let store = self.store();
        store
            .get_setting(SETTING_SYSTEM_PROMPT)
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| self.cfg.default_system_prompt.clone())
    }

    /// Снимок настроек для команды /settings: (поля, переопределённые ключи).
    pub fn settings_snapshot(&self) -> SettingsSnapshot {
        let eff = self.effective_settings();
        let prompt = self.effective_system_prompt();
        let store = self.store();
        let overridden: Vec<String> = store.all_settings().unwrap_or_default();
        SettingsSnapshot {
            base_url: eff.base_url,
            api_key: eff.api_key,
            model: eff.model,
            system_prompt: prompt,
            default_base_url: self.cfg.llm_base_url.clone(),
            default_model: self.cfg.llm_model.clone(),
            default_system_prompt: self.cfg.default_system_prompt.clone(),
            overridden,
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.store().set_setting(key, value).map_err(|e| e.to_string())
    }

    pub fn reset_setting(&self, key: &str) -> Result<bool, String> {
        self.store().delete_setting(key).map_err(|e| e.to_string())
    }
}

/// Снимок настроек для отображения в /settings.
pub struct SettingsSnapshot {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub system_prompt: String,
    pub default_base_url: String,
    pub default_model: String,
    pub default_system_prompt: String,
    /// Ключи, переопределённые в БД (llm_base_url, llm_api_key, llm_model, system_prompt).
    pub overridden: Vec<String>,
}

impl SettingsSnapshot {
    pub fn is_overridden(&self, key: &str) -> bool {
        self.overridden.iter().any(|k| k == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_with_and_without_guides() {
        let base = "Ты умный бот.";
        let plain = build_system_prompt(base, &[]);
        assert_eq!(plain, base);

        let with = build_system_prompt(base, &[("rust".into(), "Cargo.toml".into())]);
        assert!(with.starts_with(base));
        assert!(with.contains("Из гайда «rust»"));
        assert!(with.contains("Cargo.toml"));
    }

    #[tokio::test]
    async fn rate_limiter_allows_up_to_limit() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        assert!(limiter.check_and_record(1).await);
        assert!(limiter.check_and_record(1).await);
        assert!(limiter.check_and_record(1).await);
        // 4-й — превышение лимита
        assert!(!limiter.check_and_record(1).await);
    }

    #[tokio::test]
    async fn rate_limiter_isolates_users() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        assert!(limiter.check_and_record(1).await);
        assert!(limiter.check_and_record(1).await);
        assert!(!limiter.check_and_record(1).await);
        // Пользователь 2 имеет отдельный лимит
        assert!(limiter.check_and_record(2).await);
        assert!(limiter.check_and_record(2).await);
        assert!(!limiter.check_and_record(2).await);
    }

    #[tokio::test]
    async fn rate_limiter_remaining_decrements() {
        let limiter = RateLimiter::new(5, Duration::from_secs(60));
        assert_eq!(limiter.remaining(42).await, 5);
        limiter.check_and_record(42).await;
        assert_eq!(limiter.remaining(42).await, 4);
        limiter.check_and_record(42).await;
        assert_eq!(limiter.remaining(42).await, 3);
    }

    #[tokio::test]
    async fn rate_limiter_records_atomically() {
        // Два параллельных запроса от одного пользователя с лимитом 1:
        // только один должен пройти.
        let limiter = RateLimiter::new(1, Duration::from_secs(60));
        let (a, b) = tokio::join!(
            limiter.check_and_record(99),
            limiter.check_and_record(99),
        );
        assert!(a ^ b, "только один из двух запросов должен быть разрешён");
    }

    #[test]
    fn chat_token_budget_progression() {
        let base = 5000usize;
        let max = 40000usize;
        let step = (max - base) / 5;

        let budget = |n: usize| -> usize {
            if n <= 1 || max <= base {
                return base;
            }
            (base + (n - 1) * step).min(max)
        };

        assert_eq!(budget(1), 5000);
        assert_eq!(budget(2), 12000);
        assert_eq!(budget(3), 19000);
        assert_eq!(budget(4), 26000);
        assert_eq!(budget(5), 33000);
        assert_eq!(budget(6), 40000);
        assert_eq!(budget(10), 40000, "capped at max");
    }
}
