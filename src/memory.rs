use std::sync::{Arc, Mutex, MutexGuard};

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

fn build_system_prompt(guide_hits: &[(String, String)]) -> String {
    let mut prompt = String::from(
        "Ты — KostubetAI, дружелюбный и сообразительный ИИ-собеседник в Telegram.\n\
         Правила:\n\
         — Отвечай на языке собеседника (по умолчанию — русский).\n\
         — Пиши простым текстом: без Markdown-разметки (звёздочек, решёток, обратных кавычек), Telegram её не отображает.\n\
         — Отвечай по существу и живо; в перечислениях используй строки с дефисом.\n\
         — Ты видишь последние сообщения этого пользователя. Опирайся на них, чтобы помнить контекст беседы.\n\
         — Не выдумывай факты, которых нет в сообщениях или в выдержках из гайдов ниже.",
    );
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

/// Менеджер памяти. Хранит для каждого пользователя только дословную историю
/// последних сообщений в пределах лимита токенов (по умолчанию 5000) — ничего
/// лишнего не запоминается и не пересказывается. При каждом вопросе в контекст
/// также подмешиваются релевантные выдержки из загруженных гайдов.
#[derive(Clone)]
pub struct MemoryManager {
    store: Arc<Mutex<MemoryStore>>,
    llm: LlmClient,
    cfg: Arc<Config>,
}

impl MemoryManager {
    pub fn new(store: Arc<Mutex<MemoryStore>>, llm: LlmClient, cfg: Arc<Config>) -> Self {
        Self { store, llm, cfg }
    }

    fn store(&self) -> MutexGuard<'_, MemoryStore> {
        self.store.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Главная функция: записать сообщение пользователя, собрать контекст из его
    /// личной истории и релевантных гайдов, получить ответ и обрезать историю
    /// до лимита токенов. Сообщение пользователя сохраняется до вызова модели,
    /// чтобы даже при сбое ответа оно осталось в памяти.
    pub async fn reply(&self, user_id: i64, text: &str) -> Result<String, String> {
        let user_content: String = if text.chars().count() > 15_000 {
            let mut cut: String = text.chars().take(15_000).collect();
            cut.push_str("\n[сообщение обрезано из-за длины]");
            cut
        } else {
            text.to_string()
        };

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

        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(ChatMessage::system(build_system_prompt(&guide_hits)));
        for m in history {
            if m.role == "assistant" {
                messages.push(ChatMessage::assistant(m.content));
            } else {
                messages.push(ChatMessage::user(m.content));
            }
        }

        // Эффективные настройки: переопределение из БД (админ-панель) либо .env.
        let settings = self.effective_settings();
        let answer = self
            .llm
            .chat(&settings, &messages, self.cfg.max_reply_tokens, self.cfg.temperature)
            .await
            .map_err(|e| format!("{e}"))?;

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

    /// Дёшев проверяет общий размер базы; при превышении 95% лимита чистит.
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

    // ===== Настройки LLM (админ-панель) =====

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

    /// Снимок настроек для команды /settings: (поля, переопределённые ключи).
    pub fn settings_snapshot(&self) -> SettingsSnapshot {
        let eff = self.effective_settings();
        let store = self.store();
        let overridden: Vec<String> = store.all_settings().unwrap_or_default();
        SettingsSnapshot {
            base_url: eff.base_url,
            api_key: eff.api_key,
            model: eff.model,
            default_base_url: self.cfg.llm_base_url.clone(),
            default_model: self.cfg.llm_model.clone(),
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
    pub default_base_url: String,
    pub default_model: String,
    /// Ключи, переопределённые в БД (llm_base_url, llm_api_key, llm_model).
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
        let plain = build_system_prompt(&[]);
        assert!(plain.contains("KostubetAI"));
        assert!(!plain.contains("гайда"));

        let with = build_system_prompt(&[("rust".into(), "Cargo.toml".into())]);
        assert!(with.contains("Из гайда «rust»"));
        assert!(with.contains("Cargo.toml"));
    }
}
