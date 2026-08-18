use std::collections::HashSet;
use std::env;

/// Конфигурация бота, загружаемая из переменных окружения (или файла `.env`).
#[derive(Clone, Debug)]
pub struct Config {
    pub bot_token: String,
    pub llm_api_key: String,
    pub llm_base_url: String,
    pub llm_model: String,
    pub database_path: String,
    /// Жёсткий лимит размера всей базы памяти (по умолчанию 1 ГБ).
    pub memory_limit_bytes: u64,
    /// Сколько токенов истории максимум помнится на одного пользователя.
    pub user_memory_tokens: usize,
    /// Пусто — управлять гайдами может любой; иначе только перечисленные user_id.
    pub admins: HashSet<i64>,
    pub max_reply_tokens: u32,
    pub temperature: f32,
    /// Чат(ы), в которых бот активен. Пусто — бот работает в любом чате.
    /// Поддерживаются и суперчаты (группы), и личные чаты.
    pub allowed_chats: HashSet<i64>,
    /// Темы (топики) форум-супергрупп, в которых бот активен.
    /// Пусто — бот работает во всех темах. Применяется только к чатам с
    /// включёнными темами (is_forum); обычные группы и лички не затрагиваются.
    pub allowed_threads: HashSet<i64>,
}

fn var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn var_or(key: &str, default: &str) -> String {
    var(key).unwrap_or_else(|| default.to_string())
}

fn parse_num<T: std::str::FromStr>(key: &str, default: &str) -> Result<T, String> {
    var_or(key, default)
        .parse::<T>()
        .map_err(|_| format!("неверное значение переменной {key}: ожидается число"))
}

/// Парсит список целых чисел через запятую из переменной окружения `key`.
fn parse_id_list(key: &str) -> HashSet<i64> {
    var(key)
        .map(|list| {
            list.split(',')
                .filter_map(|id| id.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Если переменная задана и непуста, но не распарсилась ни одна id — ошибка.
fn validate_id_list(key: &str, set: &HashSet<i64>, what: &str) -> Result<(), String> {
    if !var(key).map(|v| v.is_empty()).unwrap_or(true) && set.is_empty() {
        return Err(format!(
            "{key} задан, но не содержит корректных числовых {what}"
        ));
    }
    Ok(())
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bot_token = var("TELEGRAM_BOT_TOKEN")
            .ok_or("не задана переменная TELEGRAM_BOT_TOKEN (токен от @BotFather)")?;

        let memory_limit_mb: u64 = parse_num("MEMORY_LIMIT_MB", "1024")?;
        if memory_limit_mb == 0 {
            return Err("MEMORY_LIMIT_MB должен быть больше 0".into());
        }

        let user_memory_tokens: usize = parse_num("USER_MEMORY_TOKENS", "5000")?;
        let max_reply_tokens: u32 = parse_num("MAX_REPLY_TOKENS", "1500")?;
        let temperature: f32 = parse_num("LLM_TEMPERATURE", "0.7")?;

        let admins = parse_id_list("ADMIN_USER_IDS");
        validate_id_list("ADMIN_USER_IDS", &admins, "user_id")?;

        let llm_api_key = var_or("LLM_API_KEY", "");
        if llm_api_key.is_empty() {
            // Допустимо для локальных серверов без авторизации (Ollama и т.п.)
            tracing::warn!("LLM_API_KEY пуст — запросы пойдут без заголовка авторизации");
        }

        let allowed_chats = parse_id_list("ALLOWED_CHATS");
        validate_id_list("ALLOWED_CHATS", &allowed_chats, "chat_id")?;

        let allowed_threads = parse_id_list("ALLOWED_THREADS");
        validate_id_list("ALLOWED_THREADS", &allowed_threads, "thread_id")?;

        Ok(Config {
            bot_token,
            llm_api_key,
            llm_base_url: var_or("LLM_BASE_URL", "https://api.openai.com/v1"),
            llm_model: var_or("LLM_MODEL", "gpt-4o-mini"),
            database_path: var_or("DATABASE_PATH", "data/memory.db"),
            memory_limit_bytes: memory_limit_mb * 1024 * 1024,
            user_memory_tokens: user_memory_tokens.clamp(100, 200_000),
            admins,
            max_reply_tokens: max_reply_tokens.clamp(64, 16_000),
            temperature: temperature.clamp(0.0, 2.0),
            allowed_chats,
            allowed_threads,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_list_reads_comma_separated() {
        std::env::set_var("KOSTUBETAI_TEST_IDS", "123, -456 ,789");
        let set = parse_id_list("KOSTUBETAI_TEST_IDS");
        assert!(set.contains(&123));
        assert!(set.contains(&-456));
        assert!(set.contains(&789));
        assert_eq!(set.len(), 3);
        std::env::remove_var("KOSTUBETAI_TEST_IDS");
    }

    #[test]
    fn parse_id_list_empty_when_unset() {
        std::env::remove_var("KOSTUBETAI_TEST_NONE");
        assert!(parse_id_list("KOSTUBETAI_TEST_NONE").is_empty());
    }

    #[test]
    fn validate_id_list_rejects_garbage() {
        std::env::set_var("KOSTUBETAI_TEST_GARBAGE", "abc, def");
        let set = parse_id_list("KOSTUBETAI_TEST_GARBAGE");
        let err = validate_id_list("KOSTUBETAI_TEST_GARBAGE", &set, "x");
        assert!(err.is_err());
        std::env::remove_var("KOSTUBETAI_TEST_GARBAGE");
    }

    #[test]
    fn validate_id_list_ok_when_unset() {
        std::env::remove_var("KOSTUBETAI_TEST_NONE2");
        let set = parse_id_list("KOSTUBETAI_TEST_NONE2");
        assert!(validate_id_list("KOSTUBETAI_TEST_NONE2", &set, "x").is_ok());
    }
}
