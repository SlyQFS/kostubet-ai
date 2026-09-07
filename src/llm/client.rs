use std::time::Duration;

use futures_util::StreamExt;
use tracing::{debug, warn};

/// Одно сообщение в запросе к LLM.
#[derive(Debug, Clone)]
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

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("сеть/транспорт: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API вернул {status}: {body}")]
    Api { status: u16, body: String },
    #[error("модель вернула пустой ответ")]
    Empty,
}

impl LlmError {
    /// Можно ли повторить запрос (сетевые сбои, 429 и 5xx).
    fn retryable(&self) -> bool {
        match self {
            LlmError::Http(_) => true,
            LlmError::Api { status, .. } => *status == 429 || *status >= 500,
            LlmError::Empty => true,
        }
    }

    /// Короткая человекочитательная формулировка для чата.
    pub fn user_hint(&self) -> &'static str {
        match self {
            LlmError::Http(_) => "Не получилось связаться с моделью, попробуй ещё раз.",
            LlmError::Api { status: 429, .. } => "Модель перегружена, попробуй через минуту.",
            LlmError::Api { .. } => "Модель ответила ошибкой, попробуй ещё раз.",
            LlmError::Empty => "Модель вернула пустой ответ.",
        }
    }
}

const MAX_RETRIES: u32 = 2;
const BACKOFF_BASE_MS: u64 = 1500;

/// OpenAI-совместимый клиент `/chat/completions` со стримингом и ретраями.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl LlmClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    fn chat_url(&self) -> String {
        if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url)
        }
    }

    fn body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        max_tokens: usize,
        temperature: f32,
        stream: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "model": model,
            "messages": messages.iter().map(|m| serde_json::json!({
                "role": m.role, "content": m.content
            })).collect::<Vec<_>>(),
            "max_tokens": max_tokens,
            "temperature": temperature,
            "stream": stream,
        })
    }

    fn parse_api_error(status: u16, body: &str) -> LlmError {
        // Вытаскиваем человекочитаемое сообщение из JSON-ошибки провайдера, если есть.
        let msg = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v["error"]["message"]
                    .as_str()
                    .or_else(|| v["message"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| body.chars().take(300).collect());
        LlmError::Api { status, body: msg }
    }

    /// Генерация со стримингом: `on_delta` вызывается по мере поступления кусков.
    /// Пустой стрим автоматически повторяется как не-стриминговый запрос.
    pub async fn stream_chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        max_tokens: usize,
        temperature: f32,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<String, LlmError> {
        let mut attempt: u32 = 0;
        loop {
            let mut full = String::new();

            let last_err: Option<LlmError> = match self
                .attempt_stream(model, messages, max_tokens, temperature, &mut full, on_delta)
                .await
            {
                Ok(()) if !full.trim().is_empty() => return Ok(full),
                Ok(()) => {
                    // Стрим отдал ноль контента — пробуем обычный запрос.
                    debug!("пустой стрим, fallback на не-стриминговый запрос");
                    match self.attempt_plain(model, messages, max_tokens, temperature).await {
                        Ok(text) if !text.trim().is_empty() => return Ok(text),
                        Ok(_) => Some(LlmError::Empty),
                        Err(e) => Some(e),
                    }
                }
                Err(e) => {
                    warn!("стрим прервался: {e}");
                    Some(e)
                }
            };

            let err = last_err.unwrap_or(LlmError::Empty);
            if attempt < MAX_RETRIES && err.retryable() {
                let delay = BACKOFF_BASE_MS * (1 << attempt);
                warn!("попытка {} не удалась ({err}), повтор через {delay} мс", attempt + 1);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                attempt += 1;
                continue;
            }
            return Err(err);
        }
    }

    async fn attempt_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        max_tokens: usize,
        temperature: f32,
        full: &mut String,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<(), LlmError> {
        let resp = self
            .http
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&self.body(model, messages, max_tokens, temperature, true))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let st = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::parse_api_error(st, &body));
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Разбираем завершённые строки SSE.
            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                let line = line.trim();
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    return Ok(());
                }
                if payload.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                    if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                        if !delta.is_empty() {
                            on_delta(delta);
                            full.push_str(delta);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn attempt_plain(
        &self,
        model: &str,
        messages: &[ChatMessage],
        max_tokens: usize,
        temperature: f32,
    ) -> Result<String, LlmError> {
        let resp = self
            .http
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&self.body(model, messages, max_tokens, temperature, false))
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::parse_api_error(status.as_u16(), &text));
        }

        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| LlmError::Api { status: status.as_u16(), body: text.chars().take(300).collect() })?;
        Ok(v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_url_normalization() {
        let c = LlmClient::new("https://api.openai.com/v1/", "k");
        assert_eq!(c.chat_url(), "https://api.openai.com/v1/chat/completions");
        let c = LlmClient::new("https://gw.ai/v1/chat/completions", "k");
        assert_eq!(c.chat_url(), "https://gw.ai/v1/chat/completions");
    }

    #[test]
    fn api_error_message_extracted() {
        let e = LlmClient::parse_api_error(
            429,
            r#"{"error": {"message": "rate limited"}}"#,
        );
        match &e {
            LlmError::Api { status, body } => {
                assert_eq!(*status, 429);
                assert_eq!(body, "rate limited");
                assert!(e.retryable());
            }
            _ => panic!("wrong variant"),
        }
    }
}
