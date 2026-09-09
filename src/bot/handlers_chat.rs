//! Групповые чаты: триггеры, генерация с очередью и прогрессивным выводом.

use std::sync::Arc;
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::ChatAction;

use super::App;
use crate::llm::client::LlmError;
use crate::llm::prompt::{build_messages, format_knowledge, PromptParts};
use crate::storage::Role;
use crate::utils::sanitizer::{extract_user_tags, remove_bot_mention};
use crate::utils::text::{split_telegram, tokens_to_chars};

const PLACEHOLDER: &str = "💬 Думаю…";
const TG_CHUNK: usize = 3900;
const EDIT_INTERVAL: Duration = Duration::from_millis(2000);
const SEND_PAUSE: Duration = Duration::from_millis(250);
const TYPING_INTERVAL: Duration = Duration::from_secs(4);
const PREVIEW_CHARS: usize = 3800;
const MAX_PARTICIPANTS: usize = 6;

/// Вход для всех сообщений: приватные чаты — заглушки, группы — триггеры.
pub async fn message(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    if msg.chat.is_private() {
        return private_stub(bot, msg, app).await;
    }
    handle_group(bot, msg, app).await
}

async fn private_stub(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    let Some(user) = msg.from else {
        return Ok(());
    };
    let text = if app.cfg.is_admin(user.id.0 as i64) {
        "В ЛС доступны только команды: /model, /help, /start."
    } else {
        "Бот отвечает только в группах. Обратись к админу, если тебя добавили в чат."
    };
    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

async fn handle_group(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    let Some(user_id) = msg
        .from
        .as_ref()
        .filter(|u| !u.is_bot)
        .map(|u| u.id.0 as i64)
    else {
        return Ok(()); // сервисные сообщения и чужие боты
    };

    // Чужие слэш-команды игнорируются без спама.
    let Some(raw_text) = msg
        .text()
        .map(str::to_owned)
        .or_else(|| msg.caption().map(str::to_owned))
    else {
        return Ok(());
    };
    if raw_text.trim_start().starts_with('/') {
        return Ok(());
    }

    let chat_id = msg.chat.id.0;
    if !app.cfg.is_chat_allowed(chat_id) {
        return Ok(());
    }
    let thread_id = thread_of(&msg);
    if !app.cfg.is_thread_allowed(thread_id) {
        return Ok(());
    }

    if !is_triggered(&msg, &app) {
        return Ok(());
    }

    if let Some(wait) = app.limiter.check_and_record(user_id as u64).await {
        let text = format!("Слишком часто. Подожди {} с.", wait.as_secs().max(1));
        send_chat_msg(&bot, msg.chat.id, msg.thread_id, Some(msg.id), text).await.ok();
        return Ok(());
    }

    let Some(prompt_text) = build_trigger_text(&msg, &raw_text, &app) else {
        return Ok(());
    };

    // Отвечаем в фоновом режиме, чтобы не блокировать диспетчер.
    tokio::spawn(generate_and_send(bot, msg, app, prompt_text, user_id));
    Ok(())
}

/// Триггер: @упоминание бота в тексте или reply на сообщение бота.
fn is_triggered(msg: &Message, app: &App) -> bool {
    let mention = format!("@{}", app.bot_username.to_lowercase());
    if msg
        .text()
        .or_else(|| msg.caption())
        .map(|t| t.to_lowercase().contains(&mention))
        .unwrap_or(false)
    {
        return true;
    }
    msg.reply_to_message()
        .and_then(|r| r.from.as_ref())
        .map(|author| author.id.0 == app.bot_id)
        .unwrap_or(false)
}

fn thread_of(msg: &Message) -> i64 {
    msg.thread_id.map(|t| t.0.0 as i64).unwrap_or(0)
}

/// Текст запроса: без @бота, с контекстом реплая/цитаты.
fn build_trigger_text(msg: &Message, raw_text: &str, app: &App) -> Option<String> {
    let mut text = remove_bot_mention(raw_text, &app.bot_username);

    let quoted = msg.reply_to_message().and_then(|reply| {
        let body = reply
            .text()
            .or_else(|| reply.caption())
            .unwrap_or_default();
        let preview: String = body.chars().take(400).collect();
        (!preview.trim().is_empty()).then(|| {
            let author = reply
                .from
                .as_ref()
                .map(display_name_of)
                .unwrap_or_else(|| "неизвестный".to_string());
            format!("\n[В ответ на сообщение от {author}: \"{preview}\"]")
        })
    });

    if text.trim().is_empty() {
        // Просто @бот без текста, но с реплаем — просим пояснить цитату.
        quoted.as_ref()?;
        text = "Разбери сообщение выше.".to_string();
    }

    if let Some(q) = quoted {
        text.push_str(&q);
    }
    Some(text)
}

fn display_name_of(user: &teloxide::types::User) -> String {
    let full = user.full_name();
    if full.trim().is_empty() {
        user.username.clone().unwrap_or_else(|| format!("id{}", user.id))
    } else {
        full
    }
}

/// Полный цикл генерации: очередь → промпт → стрим → санитайз → вывод.
async fn generate_and_send(
    bot: Bot,
    msg: Message,
    app: Arc<App>,
    prompt_text: String,
    user_id: i64,
) {
    let chat_id = msg.chat.id;
    let thread_id = thread_of(&msg);
    let display_name = msg
        .from
        .as_ref()
        .map(display_name_of)
        .unwrap_or_default();

    let budget_chars = tokens_to_chars(app.cfg.message_window_tokens);
    let history = app
        .store
        .recent_messages(chat_id.0, thread_id, budget_chars)
        .await
        .unwrap_or_default();

    if let Err(e) = app
        .store
        .add_message(chat_id.0, thread_id, user_id, &display_name, Role::User, &prompt_text)
        .await
    {
        tracing::warn!("не сохранил сообщение: {e}");
    }

    // Участники мета-контекста: автор запроса + авторы последних сообщений.
    let mut participant_ids = vec![user_id];
    for m in history.iter().rev() {
        if m.user_id != 0 && !participant_ids.contains(&m.user_id) {
            participant_ids.push(m.user_id);
        }
        if participant_ids.len() >= MAX_PARTICIPANTS {
            break;
        }
    }
    let user_states = app.store.states_for_ids(&participant_ids).await.unwrap_or_default();

    let hits = app
        .matcher
        .match_query(
            &prompt_text,
            app.cfg.knowledge_top_n,
            app.cfg.knowledge_threshold,
            app.cfg.knowledge_max_chars,
        )
        .await;
    let knowledge = format_knowledge(
        &hits
            .iter()
            .map(|(t, c)| (t.as_str(), c.as_str()))
            .collect::<Vec<_>>(),
    );

    let current_user_msg = if user_id != 0 && !display_name.is_empty() {
        format!("{display_name} (id={user_id}): {prompt_text}")
    } else {
        prompt_text.clone()
    };

    let messages = build_messages(PromptParts {
        system_prompt: crate::config::DEFAULT_SYSTEM_PROMPT,
        knowledge: &knowledge,
        user_states: &user_states,
        history: &history,
        user_message: &current_user_msg,
    });

    let thread = msg.thread_id;
    let reply_to = Some(msg.id);
    let placeholder_id = match send_chat_msg(&bot, chat_id, thread, reply_to, PLACEHOLDER).await {
        Ok(m) => Some(m.id),
        Err(e) => {
            tracing::warn!("не отправил плейсхолдер: {e}");
            return;
        }
    };
    let typing = placeholder_id.map(|_| spawn_typing(bot.clone(), chat_id, thread));

    let _permit = app.queue.clone().acquire_owned().await;
    app.wait_pace().await;

    let buffer = Arc::new(std::sync::Mutex::new(String::new()));
    let is_streaming = app.is_streaming();
    let progress = if is_streaming {
        placeholder_id.map(|id| spawn_progress_editor(bot.clone(), chat_id, id, buffer.clone()))
    } else {
        None
    };

    let model = app.models.current();
    let llm_result = if is_streaming {
        let buf = buffer.clone();
        app.llm
            .stream_chat(
                &model,
                &messages,
                app.cfg.max_reply_tokens,
                app.cfg.temperature,
                &mut |delta| {
                    if let Ok(mut guard) = buf.lock() {
                        guard.push_str(delta);
                    }
                },
            )
            .await
    } else {
        app.llm
            .plain_chat(
                &model,
                &messages,
                app.cfg.max_reply_tokens,
                app.cfg.temperature,
            )
            .await
    };

    if let Some(task) = progress {
        task.abort();
    }
    if let Some(task) = typing {
        task.abort();
    }
    drop(_permit);

    match llm_result {
        Ok(raw) => {
            let (clean, new_states) = extract_user_tags(&raw);
            let clean = remove_bot_mention(&clean, &app.bot_username);
            let meta_cap = app.cfg.user_meta_chars;
            for mut state in new_states {
                state.goal = state.goal.chars().take(meta_cap).collect();
                state.current = state.current.chars().take(meta_cap).collect();
                if let Err(e) = app.store.upsert_state(&state).await {
                    tracing::warn!("не обновил user_state {}: {e}", state.user_id);
                }
            }
            let _ = app
                .store
                .add_message(chat_id.0, thread_id, 0, "", Role::Assistant, &clean)
                .await;
            send_final(&bot, chat_id, thread, reply_to, placeholder_id, &clean).await;
        }
        Err(e) => {
            tracing::warn!("ошибка LLM: {e}");
            show_error(&bot, chat_id, thread, reply_to, placeholder_id, &e).await;
        }
    }

    if let Err(e) = app
        .store
        .trim_window(tokens_to_chars(app.cfg.message_window_tokens))
        .await
    {
        tracing::warn!("не обрезал окно истории: {e}");
    }
}

/// Typing-индикация, пока идёт генерация.
fn spawn_typing(
    bot: Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let mut req = bot.send_chat_action(chat_id, ChatAction::Typing);
            if let Some(t) = thread_id {
                req = req.message_thread_id(t);
            }
            if req.await.is_err() {
                break;
            }
            tokio::time::sleep(TYPING_INTERVAL).await;
        }
    })
}

/// Прогрессивный вывод: редактирует плейсхолдер по мере стриминга.
fn spawn_progress_editor(
    bot: Bot,
    chat_id: ChatId,
    placeholder_id: teloxide::types::MessageId,
    buffer: Arc<std::sync::Mutex<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_len = 0usize;
        loop {
            tokio::time::sleep(EDIT_INTERVAL).await;
            let text = buffer.lock().map(|g| g.clone()).unwrap_or_default();
            if text.len() != last_len && !text.trim().is_empty() {
                last_len = text.len();
                let visible = match text.find("<user:") {
                    Some(idx) => &text[..idx],
                    None => &text,
                };
                let mut preview: String = visible.chars().take(PREVIEW_CHARS).collect();
                if visible.chars().count() > PREVIEW_CHARS {
                    preview.push_str("\n▌");
                }
                if !preview.trim().is_empty()
                    && bot
                        .edit_message_text(chat_id, placeholder_id, preview)
                        .await
                        .is_err()
                {
                    break;
                }
            }
        }
    })
}

/// Финальный вывод: первый чанк редактирует плейсхолдер, остальные — с паузой.
async fn send_final(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    reply_to: Option<teloxide::types::MessageId>,
    placeholder_id: Option<teloxide::types::MessageId>,
    text: &str,
) {
    if text.trim().is_empty() {
        show_error(bot, chat_id, thread_id, reply_to, placeholder_id, &LlmError::Empty).await;
        return;
    }
    let chunks = split_telegram(text, TG_CHUNK);
    for (i, chunk) in chunks.into_iter().enumerate() {
        if i == 0 {
            let sent = match placeholder_id {
                Some(id) => match bot.edit_message_text(chat_id, id, chunk.clone()).await {
                    Ok(_) => None,
                    Err(e) => {
                        tracing::warn!("не отредактировал плейсхолдер: {e}");
                        send_chat_msg(bot, chat_id, thread_id, reply_to, chunk).await.ok()
                    }
                },
                None => send_chat_msg(bot, chat_id, thread_id, reply_to, chunk).await.ok(),
            };
            let _ = sent;
        } else {
            tokio::time::sleep(SEND_PAUSE).await;
            if send_chat_msg(bot, chat_id, thread_id, reply_to, chunk).await.is_err() {
                break;
            }
        }
    }
}

async fn show_error(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    reply_to: Option<teloxide::types::MessageId>,
    placeholder_id: Option<teloxide::types::MessageId>,
    e: &LlmError,
) {
    let hint = e.user_hint();
    match placeholder_id {
        Some(id) => {
            if bot.edit_message_text(chat_id, id, hint).await.is_err() {
                let _ = send_chat_msg(bot, chat_id, thread_id, reply_to, hint).await;
            }
        }
        None => {
            let _ = send_chat_msg(bot, chat_id, thread_id, reply_to, hint).await;
        }
    }
}

fn send_chat_msg(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    reply_to: Option<teloxide::types::MessageId>,
    text: impl Into<String>,
) -> teloxide::requests::JsonRequest<teloxide::payloads::SendMessage> {
    let mut req = bot.send_message(chat_id, text);
    if let Some(t) = thread_id {
        req = req.message_thread_id(t);
    }
    if let Some(reply_id) = reply_to {
        req = req.reply_parameters(
            teloxide::types::ReplyParameters::new(reply_id)
                .allow_sending_without_reply(),
        );
    }
    req
}
