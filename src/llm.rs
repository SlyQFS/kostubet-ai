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
                let short: String = body.chars().take(300).collect();
                write!(f, "API вернул ошибку {status}: {short}")
            }
            LlmError::Parse(e) => write!(f, "не удалось разобрать ответ: {e}"),
            LlmError::Empty => write!(f, "модель вернула пустой ответ"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Минимальный клиент для любого OpenAI-совместимого `/chat/completions`
/// (OpenAI, OpenRouter, Ollama, vLLM, ...).
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
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
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .build()
            .expect("не удалось создать HTTP-клиент");
        Self { http, base_url: base_url.trim_end_matches('/').to_string(), api_key, model }
    }

    pub async fn chat(&self, messages: &[ChatMessage], max_tokens: u32, temperature: f32) -> Result<String, LlmError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest { model: &self.model, messages, max_tokens, temperature };

        let mut request = self.http.post(&url).json(&body);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }

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
