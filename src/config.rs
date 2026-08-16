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

        let admins: HashSet<i64> = var("ADMIN_USER_IDS")
            .map(|list| {
                list.split(',')
                    .filter_map(|id| id.trim().parse::<i64>().ok())
                    .collect()
            })
            .unwrap_or_default();
        if !var("ADMIN_USER_IDS").map(|v| v.is_empty()).unwrap_or(true) && admins.is_empty() {
            return Err("ADMIN_USER_IDS задан, но не содержит корректных числовых user_id".into());
        }

        let llm_api_key = var_or("LLM_API_KEY", "");
        if llm_api_key.is_empty() {
            // Допустимо для локальных серверов без авторизации (Ollama и т.п.)
            tracing::warn!("LLM_API_KEY пуст — запросы пойдут без заголовка авторизации");
        }

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
        })
    }
}
