use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Сообщение в формате chat-completions API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: content.into() }
    }
}

#[derive(Debug)]
pub enum LlmError {
    Http(reqwest::Error),
    Api { status: u16, body: String },
    Parse(String),
    Empty,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Http(e) => write!(f, "сетевая ошибка: {e}"),
            LlmError::Api { status, body } => {
                let clean_msg = serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| {
                        v.get("error")
                            .and_then(|e| {
                                e.get("message")
                                    .and_then(|m| m.as_str())
                                    .or_else(|| e.as_str())
                            })
                            .map(str::to_string)
                    });
                if let Some(msg) = clean_msg {
                    write!(f, "API вернул ошибку {status}: {msg}")
                } else {
                    let short: String = body.chars().take(300).collect();
                    write!(f, "API вернул ошибку {status}: {short}")
                }
            }
            LlmError::Parse(e) => write!(f, "не удалось разобрать ответ: {e}"),
            LlmError::Empty => write!(f, "модель вернула пустой ответ"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Параметры подключения к LLM API. Могут переопределяться из админ-панели
/// (хранятся в БД), поэтому передаются в каждый вызов, а не фиксируются в клиенте.
#[derive(Clone, Debug)]
pub struct LlmSettings {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Минимальный клиент для любого OpenAI-совместимого `/chat/completions`
/// (OpenAI, OpenRouter, Ollama, vLLM, ...).
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    max_tokens: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Deserialize)]
struct RespMessage {
    content: Option<String>,
}

impl LlmClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .build()
            .expect("не удалось создать HTTP-клиент");
        Self { http }
    }

    /// Нормализует базовый URL к виду эндпоинта /chat/completions.
    pub fn build_chat_url(base_url: &str) -> String {
        let base = base_url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    pub async fn chat(
        &self,
        settings: &LlmSettings,
        messages: &[ChatMessage],
        max_tokens: u32,
        temperature: f32,
    ) -> Result<String, LlmError> {
        let url = Self::build_chat_url(&settings.base_url);
        let body = ChatRequest { model: &settings.model, messages, max_tokens, temperature };

        let mut request = self.http.post(&url).json(&body);
        if !settings.api_key.is_empty() {
            request = request.bearer_auth(&settings.api_key);
        }
        // Заголовки для совместимости с OpenRouter и внешними шлюзами
        request = request
            .header("HTTP-Referer", "https://github.com/SlyQFS/kostubet-ai")
            .header("X-Title", "KostubetAI");

        let response = request.send().await.map_err(LlmError::Http)?;
        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            return Err(LlmError::Api { status: status.as_u16(), body: text });
        }

        let parsed: ChatResponse =
            serde_json::from_str(&text).map_err(|e| LlmError::Parse(e.to_string()))?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .ok_or(LlmError::Empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_chat_url_normalizes_paths() {
        assert_eq!(
            LlmClient::build_chat_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            LlmClient::build_chat_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            LlmClient::build_chat_url("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            LlmClient::build_chat_url("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn api_error_extracts_json_message() {
        let err = LlmError::Api {
            status: 401,
            body: r#"{"error": {"message": "Incorrect API key provided", "type": "invalid_request_error"}}"#.into(),
        };
        assert_eq!(err.to_string(), "API вернул ошибку 401: Incorrect API key provided");
    }
}
