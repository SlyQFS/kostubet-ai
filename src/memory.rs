use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

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

/// Память одного пользователя для команды /memory.
pub struct UserSnapshot {
    pub message_count: i64,
    pub est_tokens: usize,
}

/// Менеджер памяти и очереди запросов.
/// Хранит для каждого пользователя только дословную историю последних сообщений,
/// управляет очередью и защищает от перегрузок через семафор параллельности и блокировку на пользователя.
#[derive(Clone)]
pub struct MemoryManager {
    store: Arc<Mutex<MemoryStore>>,
    llm: LlmClient,
    cfg: Arc<Config>,
    semaphore: Arc<Semaphore>,
    user_locks: Arc<TokioMutex<HashMap<i64, Arc<TokioMutex<()>>>>>,
    last_request_time: Arc<TokioMutex<std::time::Instant>>,
}

impl MemoryManager {
    pub fn new(store: Arc<Mutex<MemoryStore>>, llm: LlmClient, cfg: Arc<Config>) -> Self {
        let max_concurrent = cfg.max_concurrent_requests;
        Self {
            store,
            llm,
            cfg,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            user_locks: Arc::new(TokioMutex::new(HashMap::new())),
            last_request_time: Arc::new(TokioMutex::new(std::time::Instant::now() - std::time::Duration::from_secs(10))),
        }
    }

    fn store(&self) -> MutexGuard<'_, MemoryStore> {
        self.store.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Получает мьютекс для конкретного пользователя, чтобы его запросы обрабатывались строго последовательно.
    async fn user_lock(&self, user_id: i64) -> Arc<TokioMutex<()>> {
        let mut locks = self.user_locks.lock().await;
        locks
            .entry(user_id)
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    /// Синхронная/нестриминговая обёртка над reply_stream.
    #[allow(dead_code)]
    pub async fn reply(&self, user_id: i64, text: &str) -> Result<String, String> {
        self.reply_stream(user_id, text, |_| {}).await
    }

    /// Стриминговая генерация ответа с передачей инкрементов текста через callback on_delta.
    pub async fn reply_stream<F>(&self, user_id: i64, text: &str, on_delta: F) -> Result<String, String>
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

        // Захватываем блокировку пользователя для защиты от параллельного спама одного юзера
        let u_lock = self.user_lock(user_id).await;
        let _user_guard = u_lock.lock().await;

        // Сохраняем входящее сообщение до сетевого вызова
        {
            let store = self.store();
            store
                .add_message(user_id, "user", &user_content)
                .map_err(|e| format!("ошибка базы данных: {e}"))?;
        }

        let (history, guide_hits) = {
            let store = self.store();
            let history = store
                .user_messages_within_tokens(user_id, self.cfg.user_memory_tokens)
                .map_err(|e| format!("ошибка базы данных: {e}"))?;
            let guide_hits = store
                .search_guide_chunks(&meaningful_words(&user_content), GUIDE_CONTEXT_CHARS)
                .unwrap_or_default();
            (history, guide_hits)
        };

        let base_prompt = self.effective_system_prompt();
        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(ChatMessage::system(build_system_prompt(&base_prompt, &guide_hits)));
        for m in history {
            if m.role == "assistant" {
                messages.push(ChatMessage::assistant(m.content));
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

        // Пейсер запросов: сглаживает залповые вопросы (burst), предотвращая 403 и 429 от WAF
        {
            let mut last = self.last_request_time.lock().await;
            let min_interval = std::time::Duration::from_millis(1500);
            let elapsed = last.elapsed();
            if elapsed < min_interval {
                tokio::time::sleep(min_interval - elapsed).await;
            }
            *last = std::time::Instant::now();
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
                .add_message(user_id, "assistant", answer.trim())
                .map_err(|e| format!("ошибка базы данных: {e}"))?;
            match store.trim_user_history(user_id, self.cfg.user_memory_tokens) {
                Ok(deleted) if deleted > 0 => {
                    tracing::debug!(user_id, deleted, "старые сообщения вышли за лимит памяти и удалены");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("не удалось обрезать историю пользователя {user_id}: {e}"),
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

    pub fn reset(&self, user_id: i64) -> Result<(), String> {
        self.store().reset_user(user_id).map_err(|e| e.to_string())
    }

    pub fn user_snapshot(&self, user_id: i64) -> UserSnapshot {
        let (message_count, est_tokens) = self.store().user_stats(user_id).unwrap_or((0, 0));
        UserSnapshot { message_count, est_tokens }
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
}
