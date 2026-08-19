use std::sync::Arc;
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{Chat, ChatAction, ChatId, Document, ReplyParameters, UserId};
use teloxide::utils::command::BotCommands;

use crate::config::Config;
use crate::memory::MemoryManager;
use crate::text::{fmt_bytes, split_telegram};

/// Максимальный размер текстового файла-гайда.
const MAX_GUIDE_FILE_BYTES: u64 = 5 * 1024 * 1024;

const HELP_ADMIN_TEXT: &str = "\
Привет! Я KostubetAI — официальный ИИ-ассистент, созданный @slyqfs 🧠

В личных сообщениях доступны команды управления и загрузка гайдов.
Общение с нейросетью работает в группах/суперчатах (при упоминании бота или ответе на его сообщение).

Команды управления (только для администраторов):
/reset — очистить память о диалоге
/memory — статистика памяти и базы
/guides — список сохранённых гайдов
/guide_add <название> — добавить гайд (ответь этой командой на сообщение с текстом)
/guide_del <название или id> — удалить гайд

Настройки LLM API и промпта:
/settings — текущие настройки API, модели и промпта
/set_model <модель> — сменить модель (например gpt-4o-mini)
/set_api <base_url> — сменить адрес API (например https://api.openai.com/v1)
/set_key <ключ> — задать API-ключ
/set_prompt <текст> — задать системный промпт
/reset_setting <model|api|key|prompt|all> — сбросить настройку к значению из .env

Гайд можно также загрузить файлом: просто пришли текстовый документ (.txt, .md, .json, ...) с названием в подписи.";

const HELP_USER_TEXT: &str = "\
Привет! Я KostubetAI — официальный ИИ-ассистент, созданный @slyqfs 🧠

В личных сообщениях я не веду диалог. Чтобы пообщаться со мной, добавь меня в группу или суперчат и задай вопрос через тег (@имя_бота) или ответь (reply) на моё сообщение!";

pub struct AppState {
    pub mgr: MemoryManager,
    pub bot_id: UserId,
    pub bot_username: Option<String>,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
}

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "snake_case", description = "Команды KostubetAI:")]
pub enum Command {
    #[command(description = "приветствие и справка")]
    Start,
    #[command(description = "справка")]
    Help,
    #[command(description = "очистить память чата (админ)")]
    Reset,
    #[command(description = "статистика памяти (админ)")]
    Memory,
    #[command(description = "список гайдов (админ)")]
    Guides,
    #[command(description = "добавить гайд: /guide_add <название> (админ)")]
    GuideAdd(String),
    #[command(description = "удалить гайд: /guide_del <название или id> (админ)")]
    GuideDel(String),
    #[command(description = "настройки API и модели (админ)")]
    Settings,
    #[command(description = "задать модель: /set_model <название> (админ)")]
    SetModel(String),
    #[command(description = "задать адрес API: /set_api <url> (админ)")]
    SetApi(String),
    #[command(description = "задать API-ключ: /set_key <ключ> (админ)")]
    SetKey(String),
    #[command(description = "задать системный промпт: /set_prompt <текст> (админ)")]
    SetPrompt(String),
    #[command(description = "задать системный промпт (админ)")]
    SetSystemPrompt(String),
    #[command(description = "сбросить настройку: /reset_setting <название> (админ)")]
    ResetSetting(String),
    #[command(description = "суммаризация диалога")]
    Summary,
}

/// Список команд для Telegram-меню в личных сообщениях.
pub fn bot_commands() -> Vec<teloxide::types::BotCommand> {
    use teloxide::types::BotCommand;
    vec![
        BotCommand::new("start", "приветствие и справка"),
        BotCommand::new("help", "справка"),
        BotCommand::new("reset", "очистить память чата (админ)"),
        BotCommand::new("memory", "статистика памяти (админ)"),
        BotCommand::new("guides", "список гайдов (админ)"),
        BotCommand::new("guide_add", "добавить гайд ответом на сообщение (админ)"),
        BotCommand::new("guide_del", "удалить гайд (админ)"),
        BotCommand::new("settings", "настройки API, модели и промпта (админ)"),
        BotCommand::new("set_model", "задать модель (админ)"),
        BotCommand::new("set_api", "задать адрес API (админ)"),
        BotCommand::new("set_key", "задать API-ключ (админ)"),
        BotCommand::new("set_prompt", "задать системный промпт (админ)"),
        BotCommand::new("reset_setting", "сбросить настройку (админ)"),
    ]
}

/// Сообщение о переадресации команд из групп в личные сообщения.
fn pm_redirect_message(bot_username: Option<&str>) -> String {
    if let Some(username) = bot_username {
        format!("ℹ️ Команды и настройки бота доступны только в личных сообщениях: @{username}")
    } else {
        "ℹ️ Команды и настройки бота доступны только в личных сообщениях с ботом.".to_string()
    }
}

pub async fn cmd_handler(bot: Bot, msg: Message, cmd: Command, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    let admin = is_admin(&state, user_id);
    let cmd_name = match &cmd {
        Command::Start => "start",
        Command::Help => "help",
        Command::Reset => "reset",
        Command::Memory => "memory",
        Command::Guides => "guides",
        Command::GuideAdd(_) => "guide_add",
        Command::GuideDel(_) => "guide_del",
        Command::Settings => "settings",
        Command::SetModel(_) => "set_model",
        Command::SetApi(_) => "set_api",
        Command::SetKey(_) => "set_key",
        Command::SetPrompt(_) | Command::SetSystemPrompt(_) => "set_prompt",
        Command::ResetSetting(_) => "reset_setting",
        Command::Summary => "summary",
    };
    tracing::info!(user_id, chat_id = msg.chat.id.0, admin, cmd = cmd_name, "обработка команды");

    if !is_allowed(&state, &msg) {
        tracing::info!(user_id, chat_id = msg.chat.id.0, "команда проигнорирована: чат или топик не в белом списке");
        return Ok(());
    }

    // В группах все команды переадресуются в личные сообщения
    if !msg.chat.is_private() {
        bot.send_message(msg.chat.id, pm_redirect_message(state.bot_username.as_deref())).await?;
        return Ok(());
    }

    match cmd {
        Command::Start | Command::Help => {
            let text = if admin { HELP_ADMIN_TEXT } else { HELP_USER_TEXT };
            bot.send_message(msg.chat.id, text).await?;
        }
        Command::Reset => {
            if !admin {
                bot.send_message(msg.chat.id, "🔒 Эта команда доступна только администратору бота.").await?;
                return Ok(());
            }
            match state.mgr.reset_chat(msg.chat.id.0) {
                Ok(()) => {
                    bot.send_message(msg.chat.id, "🧠 Готово: память чата очищена. Гайды сохранены — список: /guides").await?;
                }
                Err(e) => {
                    tracing::error!("ошибка сброса памяти: {e}");
                    bot.send_message(msg.chat.id, "⚠️ Не удалось очистить память.").await?;
                }
            }
        }
        Command::Memory => {
            if !admin {
                bot.send_message(msg.chat.id, "🔒 Эта команда доступна только администратору бота.").await?;
                return Ok(());
            }
            let snap = state.mgr.chat_snapshot(msg.chat.id.0);
            let (size, total_msgs, users, guides) = state.mgr.global_stats();
            let percent_chat = if snap.budget > 0 { snap.est_tokens as f64 / snap.budget as f64 * 100.0 } else { 0.0 };
            let percent_db = if state.cfg.memory_limit_bytes > 0 { size as f64 / state.cfg.memory_limit_bytes as f64 * 100.0 } else { 0.0 };
            let text = format!(
                "💬 Память чата: {} сообщений, ~{} из {} токенов ({:.0}%)\n\
                 👥 Участников диалога: {}\n\
                 💾 База всего: {} из {} ({:.1}%), сообщений: {}, пользователей: {}, гайдов: {}\n\n\
                 История хранится дословно, без искажающих суммаризаций.",
                snap.message_count,
                snap.est_tokens,
                snap.budget,
                percent_chat,
                snap.participants,
                fmt_bytes(size),
                fmt_bytes(state.cfg.memory_limit_bytes),
                percent_db,
                total_msgs,
                users,
                guides,
            );
            bot.send_message(msg.chat.id, text).await?;
        }
        Command::Guides => {
            if !admin {
                bot.send_message(msg.chat.id, "🔒 Эта команда доступна только администратору бота.").await?;
                return Ok(());
            }
            let guides = state.mgr.guides();
            let text = if guides.is_empty() {
                "📚 Гайдов пока нет.\n\nЗагрузить: пришли текстовый файл или ответь /guide_add <название> на сообщение с текстом.".to_string()
            } else {
                let mut out = format!("📚 Гайдов: {}\n\n", guides.len());
                for g in guides {
                    out.push_str(&format!("{}. {} (~{} симв.)\n", g.id, g.title, g.chars));
                }
                out.push_str("\nУдалить: /guide_del <название или id>");
                out
            };
            bot.send_message(msg.chat.id, text).await?;
        }
        Command::GuideAdd(title) => {
            handle_guide_add(bot, msg, &title, state).await?;
        }
        Command::GuideDel(query) => {
            handle_guide_del(bot, msg, &query, state).await?;
        }
        Command::Settings => {
            if !admin {
                bot.send_message(msg.chat.id, "🔒 Настройки доступны только администратору бота.").await?;
                return Ok(());
            }
            bot.send_message(msg.chat.id, format_settings(&state.mgr)).await?;
        }
        Command::SetModel(value) => {
            handle_set_model(bot, msg, &value, state).await?;
        }
        Command::SetApi(value) => {
            handle_set_api(bot, msg, &value, state).await?;
        }
        Command::SetKey(value) => {
            handle_set_key(bot, msg, &value, state).await?;
        }
        Command::SetPrompt(value) | Command::SetSystemPrompt(value) => {
            handle_set_prompt(bot, msg, &value, state).await?;
        }
        Command::ResetSetting(value) => {
            handle_reset_setting(bot, msg, &value, state).await?;
        }
        Command::Summary => {
            handle_summary(bot, msg, state).await?;
        }
    }
    Ok(())
}

pub async fn on_message(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    let display_name = msg.from.as_ref().map(extract_display_name).unwrap_or_default();

    if !is_allowed(&state, &msg) {
        tracing::debug!(user_id, chat_id = msg.chat.id.0, "сообщение отфильтровано: чат или топик не в белом списке");
        return Ok(());
    }
    // Документы — кандидаты на загрузку как гайд.
    if let Some(doc) = msg.document().cloned() {
        return handle_document(bot, msg, &doc, state).await;
    }

    let text_opt = msg.text().or_else(|| msg.caption()).map(ToOwned::to_owned);
    let Some(text) = text_opt else {
        return Ok(());
    };

    let text_preview: String = text.chars().take(40).collect();
    tracing::info!(user_id, chat_id = msg.chat.id.0, is_private = msg.chat.is_private(), text = %text_preview, "получено входящее сообщение");

    // Игнорируем неизвестные команды со слэшем в чатах
    if text.starts_with('/') {
        if !msg.chat.is_private() {
            bot.send_message(msg.chat.id, pm_redirect_message(state.bot_username.as_deref())).await?;
        }
        return Ok(());
    }

    let (should_reply, clean) = should_reply(&msg, &state, &text);
    if !should_reply {
        tracing::debug!(user_id, chat_id = msg.chat.id.0, "сообщение пропущено (в ЛС или в группе без обращения к боту)");
        return Ok(());
    }
    let mut user_text = if clean.is_empty() { "Привет!".to_string() } else { clean };

    // Если сообщение является ответом на другое сообщение или цитированием — дополняем контекст запроса
    if let Some(reply) = msg.reply_to_message() {
        let is_topic_header = msg.thread_id.map(|t| t.0 .0 == reply.id.0).unwrap_or(false)
            || reply.forum_topic_created().is_some();
        let is_from_bot = reply.from.as_ref().map(|u| u.id == state.bot_id).unwrap_or(false);
        if !is_topic_header && !is_from_bot {
            if let Some(reply_text) = reply.text().or_else(|| reply.caption()) {
                let reply_author = reply.from.as_ref().map(extract_display_name).unwrap_or_else(|| "Собеседник".to_string());
                let short_reply: String = reply_text.chars().take(400).collect();
                user_text = format!("[В ответ на сообщение от {reply_author}: \"{short_reply}\"]\n{user_text}");
            }
        }
    }
    if let Some(quote) = msg.quote() {
        if !user_text.contains(&quote.text) {
            let quote_text: String = quote.text.chars().take(400).collect();
            user_text = format!("[Цитата: \"{quote_text}\"]\n{user_text}");
        }
    }

    // Частотный лимит: не более N запросов за окно времени на пользователя.
    // Предотвращает монополизацию очереди одним пользователем, пока бот
    // генерирует ответ — остальные получают свои ответы без задержек.
    if !state.mgr.check_rate_limit(user_id).await {
        let max = state.mgr.rate_limit_max();
        let window_secs = state.mgr.rate_limit_window_secs();
        let window_str = if window_secs < 60 {
            format!("{window_secs} сек.")
        } else if window_secs.is_multiple_of(60) {
            format!("{} мин.", window_secs / 60)
        } else {
            format!("{:.1} мин.", window_secs as f64 / 60.0)
        };
        tracing::info!(
            user_id,
            max, window_secs,
            "запрос отклонён: превышен частотный лимит"
        );
        bot.send_message(
            msg.chat.id,
            format!(
                "⏳ Слишком частые запросы! Лимит — {max} сообщений за {window_str} Подожди немного и попробуй снова.",
            ),
        )
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;
        return Ok(());
    }

    tracing::info!(user_id, "отправка запроса в LLM (прогрессивный стриминг)...");
    let typing = spawn_typing(bot.clone(), msg.chat.id);
    let start = std::time::Instant::now();

    // Отправляем плейсхолдер для прогрессивного заполнения
    let placeholder_res = bot
        .send_message(msg.chat.id, "💬 Думаю...")
        .reply_parameters(ReplyParameters::new(msg.id))
        .await;

    let placeholder_msg = match placeholder_res {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!("не удалось отправить начальный плейсхолдер: {e}");
            None
        }
    };

    let accum = Arc::new(std::sync::Mutex::new(String::new()));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let accum_clone = accum.clone();
    let on_delta = move |delta: &str| {
        let mut a = accum_clone.lock().unwrap_or_else(|p| p.into_inner());
        a.push_str(delta);
    };

    // Фоновая задача обновления текста в сообщении раз в 1.2 секунды
    let accum_bg = accum.clone();
    let finished_bg = finished.clone();
    let bot_bg = bot.clone();
    let chat_id = msg.chat.id;
    let ph_id = placeholder_msg.as_ref().map(|m| m.id);

    let update_task = tokio::spawn(async move {
        let mut last_shown = String::new();
        let mut interval = tokio::time::interval(Duration::from_millis(1200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if finished_bg.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let current = {
                accum_bg.lock().unwrap_or_else(|p| p.into_inner()).clone()
            };
            let current_trim = current.trim();
            if !current_trim.is_empty() && current_trim != last_shown {
                last_shown = current_trim.to_string();
                if let Some(target_id) = ph_id {
                    let preview: String = current_trim.chars().take(3800).collect();
                    let _ = bot_bg.edit_message_text(chat_id, target_id, format!("{preview} ▌")).await;
                }
            }
        }
    });

    let answer = state.mgr.reply_stream(user_id, chat_id.0, &display_name, &user_text, on_delta).await;
    finished.store(true, std::sync::atomic::Ordering::Relaxed);
    update_task.abort();
    typing.abort();

    match answer {
        Ok(a) if !a.trim().is_empty() => {
            let final_text = a.trim();
            tracing::info!(user_id, elapsed_ms = start.elapsed().as_millis(), chars = final_text.len(), "ответ получен и выведен пользователю");
            if let Some(ph) = placeholder_msg {
                let chunks = split_telegram(final_text, 3900);
                if let Some(first) = chunks.first() {
                    let _ = bot.edit_message_text(chat_id, ph.id, first).await;
                }
                for chunk in chunks.iter().skip(1) {
                    let _ = bot.send_message(chat_id, chunk).await;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            } else {
                send_chunks(&bot, &msg, final_text).await?;
            }
        }
        Ok(_) => {
            tracing::warn!(user_id, "модель вернула пустой ответ");
            if let Some(ph) = placeholder_msg {
                let _ = bot.delete_message(chat_id, ph.id).await;
            }
        }
        Err(e) => {
            tracing::error!(user_id, elapsed_ms = start.elapsed().as_millis(), "ошибка генерации ответа после реконнектов: {e}");
            if let Some(ph) = placeholder_msg {
                let _ = bot.edit_message_text(chat_id, ph.id, "⚠️ Не удалось получить ответ от нейросети (сбой провайдера или лимит API). Попробуй ещё раз чуть позже.").await;
            }
        }
    }
    Ok(())
}

pub fn is_admin(state: &AppState, user_id: i64) -> bool {
    state.cfg.admins.is_empty() || state.cfg.admins.contains(&user_id)
}

/// Извлекает отображаемое имя пользователя для атрибуции в полу-разделяемой памяти.
fn extract_display_name(u: &teloxide::types::User) -> String {
    let name = if let Some(last) = &u.last_name {
        format!("{} {}", u.first_name, last)
    } else {
        u.first_name.clone()
    };
    let name = name.trim();
    if !name.is_empty() {
        name.to_string()
    } else if let Some(username) = &u.username {
        format!("@{username}")
    } else {
        format!("User{}", u.id)
    }
}

async fn handle_summary(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if is_admin(&state, user_id) {
        bot.send_message(
            msg.chat.id,
            "🧠 Команда /summary не требуется — KostubetAI помнит историю сообщений дословно, без искажающих пересказов.\n\nСтатистика памяти: /memory",
        )
        .await?;
    } else {
        bot.send_message(
            msg.chat.id,
            "🧠 Я помню контекст нашей беседы дословно. Просто продолжай диалог!",
        )
        .await?;
    }
    Ok(())
}

async fn handle_guide_add(bot: Bot, msg: Message, title: &str, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Управлять гайдами может только администратор бота.").await?;
        return Ok(());
    }
    let title = title.trim();
    if title.is_empty() {
        bot.send_message(
            msg.chat.id,
            "Укажи название: /guide_add Моё название — и ответь этой командой на сообщение с текстом гайда.",
        )
        .await?;
        return Ok(());
    }
    let content = msg.reply_to_message().and_then(|m| m.text().or_else(|| m.caption()));
    match content {
        Some(content) => match state.mgr.add_guide(title, user_id, content) {
            Ok(chunks) => {
                tracing::info!(user_id, title, chunks, "добавлен гайд");
                bot.send_message(msg.chat.id, format!("📚 Гайд «{title}» сохранён ({chunks} фрагментов).")).await?;
            }
            Err(e) => {
                bot.send_message(msg.chat.id, format!("⚠️ {e}")).await?;
            }
        },
        None => {
            bot.send_message(
                msg.chat.id,
                "Ответь этой командой на сообщение, текст которого нужно сохранить как гайд.",
            )
            .await?;
        }
    }
    Ok(())
}

async fn handle_guide_del(bot: Bot, msg: Message, query: &str, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Управлять гайдами может только администратор бота.").await?;
        return Ok(());
    }
    let query = query.trim();
    if query.is_empty() {
        bot.send_message(msg.chat.id, "Укажи гайд: /guide_del <название или id>. Список: /guides").await?;
        return Ok(());
    }
    match state.mgr.find_guide(query) {
        Some(id) if state.mgr.delete_guide(id) => {
            tracing::info!(user_id, id, "удалён гайд");
            bot.send_message(msg.chat.id, "🗑 Гайд удалён.").await?;
        }
        _ => {
            bot.send_message(msg.chat.id, "Гайд не найден. Список: /guides").await?;
        }
    }
    Ok(())
}

fn is_text_document(doc: &Document) -> bool {
    if let Some(mime) = &doc.mime_type {
        if mime.essence_str().starts_with("text/") {
            return true;
        }
    }
    doc.file_name
        .as_deref()
        .map(|name| {
            let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
            matches!(
                ext.as_str(),
                "txt" | "md" | "markdown" | "csv" | "json" | "log" | "yaml" | "yml" | "toml" | "rst" | "xml" | "html" | "patch"
            )
        })
        .unwrap_or(false)
}

async fn handle_document(bot: Bot, msg: Message, doc: &Document, state: Arc<AppState>) -> ResponseResult<()> {
    let Some(user_id) = msg.from.as_ref().map(|u| u.id.0 as i64) else {
        return Ok(());
    };

    let caption = msg.caption().map(ToOwned::to_owned).unwrap_or_default();

    // В группах загрузка гайдов через документы переадресуется в личные сообщения
    if !msg.chat.is_private() {
        bot.send_message(msg.chat.id, pm_redirect_message(state.bot_username.as_deref())).await?;
        return Ok(());
    }
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Загружать гайды может только администратор бота.").await?;
        return Ok(());
    }

    if doc.file.size as u64 > MAX_GUIDE_FILE_BYTES {
        bot.send_message(msg.chat.id, "⚠️ Файл слишком большой, лимит — 5 МБ.").await?;
        return Ok(());
    }
    if !is_text_document(doc) {
        bot.send_message(
            msg.chat.id,
            "⚠️ Принимаю только текстовые файлы: .txt, .md, .csv, .json, .log, .yaml и похожие.",
        )
        .await?;
        return Ok(());
    }

    // Скачиваем через файловый API Telegram.
    let file = match bot.get_file(doc.file.id.clone()).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("get_file не удался: {e}");
            bot.send_message(msg.chat.id, "⚠️ Не удалось получить файл от Telegram.").await?;
            return Ok(());
        }
    };
    let url = format!("https://api.telegram.org/file/bot{}/{}", state.cfg.bot_token, file.path);
    let content = match download_text(&state.http, &url).await {
        Ok(text) => text,
        Err(e) => {
            tracing::error!("скачивание файла не удалось: {e}");
            bot.send_message(msg.chat.id, "⚠️ Не удалось скачать файл.").await?;
            return Ok(());
        }
    };

    let base_name = doc
        .file_name
        .as_deref()
        .map(|n| n.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(n))
        .filter(|n| !n.trim().is_empty());
    let title: String = caption
        .trim()
        .split('\n')
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(64)
        .collect();
    let title = if title.is_empty() { base_name.map(|n| n.chars().take(64).collect()).unwrap_or_default() } else { title };

    if title.trim().is_empty() {
        bot.send_message(
            msg.chat.id,
            "Укажи название гайда в подписи к файлу (первая строка) и пришли файл ещё раз.",
        )
        .await?;
        return Ok(());
    }

    match state.mgr.add_guide(&title, user_id, &content) {
        Ok(chunks) => {
            tracing::info!(user_id, title, chunks, "добавлен гайд из файла");
            bot.send_message(msg.chat.id, format!("📚 Гайд «{title}» сохранён ({chunks} фрагментов).")).await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("⚠️ {e}")).await?;
        }
    }
    Ok(())
}

/// Скачивает файл и читает его как текст (некорректные байты заменяются, а не роняют загрузку).
async fn download_text(http: &reqwest::Client, url: &str) -> Result<String, reqwest::Error> {
    let bytes = http.get(url).send().await?.error_for_status()?.bytes().await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn spawn_typing(bot: Bot, chat_id: ChatId) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(4));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;
        }
    })
}

async fn send_chunks(bot: &Bot, msg: &Message, text: &str) -> ResponseResult<()> {
    for (i, chunk) in split_telegram(text, 3900).into_iter().enumerate() {
        let mut request = bot.send_message(msg.chat.id, chunk);
        if i == 0 {
            request = request.reply_parameters(ReplyParameters::new(msg.id));
        }
        request.await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

/// Проверяет, разрешён ли чат для работы бота.
fn chat_allowed(state: &AppState, msg: &Message) -> bool {
    if state.cfg.allowed_chats.is_empty() {
        return true;
    }
    if state.cfg.allowed_chats.contains(&msg.chat.id.0) {
        return true;
    }
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if msg.chat.is_private() && is_admin(state, user_id) {
        return true;
    }
    false
}

/// Проверяет, разрешена ли тема (топик) форум-чата для работы бота.
fn thread_allowed(state: &AppState, msg: &Message) -> bool {
    if state.cfg.allowed_threads.is_empty() || !is_forum(&msg.chat) {
        return true;
    }
    msg.thread_id
        .map(|t| state.cfg.allowed_threads.contains(&(t.0 .0 as i64)))
        .unwrap_or(false)
}

/// true для супергруппы с включёнными темами (форум).
fn is_forum(chat: &Chat) -> bool {
    matches!(
        chat.kind,
        teloxide::types::ChatKind::Public(teloxide::types::ChatPublic {
            kind: teloxide::types::PublicChatKind::Supergroup(ref sg),
            ..
        }) if sg.is_forum
    )
}

/// Объединённая проверка: чат + тема. Вызывается в начале обработчиков.
fn is_allowed(state: &AppState, msg: &Message) -> bool {
    chat_allowed(state, msg) && thread_allowed(state, msg)
}

/// В приватных чатах (ЛС) нейросетью не отвечаем (доступны только команды);
/// в группах — отвечаем при упоминании @бота или ответе (reply / quote) на сообщение бота.
pub fn should_reply(msg: &Message, state: &AppState, text: &str) -> (bool, String) {
    if msg.chat.is_private() {
        return (false, String::new());
    }
    let mut mentioned = false;
    let mut clean = text.to_string();

    // 1. Поиск тега @bot_username в тексте или подписи
    if let Some(username) = &state.bot_username {
        let tag = format!("@{username}");
        let mut words_out = Vec::new();
        for word in clean.split_whitespace() {
            if word.eq_ignore_ascii_case(&tag) {
                mentioned = true;
            } else if word.to_lowercase().starts_with(&tag.to_lowercase()) {
                mentioned = true;
                let remainder = &word[tag.len()..];
                if !remainder.is_empty() {
                    words_out.push(remainder);
                }
            } else {
                words_out.push(word);
            }
        }
        clean = words_out.join(" ");
    }

    // 2. Проверка ответа (reply) на сообщение бота
    let replied_to_bot = if let Some(reply) = msg.reply_to_message() {
        // Исключаем служебное сообщение о создании форум-топика
        let is_topic_header = msg.thread_id.map(|t| t.0 .0 == reply.id.0).unwrap_or(false)
            || reply.forum_topic_created().is_some();

        if is_topic_header {
            false
        } else {
            let from_bot = reply
                .from
                .as_ref()
                .map(|u| {
                    u.id == state.bot_id
                        || state
                            .bot_username
                            .as_deref()
                            .map(|b| u.username.as_deref().map(|un| un.eq_ignore_ascii_case(b)).unwrap_or(false))
                            .unwrap_or(false)
                })
                .unwrap_or(false);

            let via_bot = reply
                .via_bot
                .as_ref()
                .map(|u| u.id == state.bot_id)
                .unwrap_or(false);

            let sender_chat_bot = reply
                .sender_chat
                .as_ref()
                .map(|c| c.id.0 == state.bot_id.0 as i64)
                .unwrap_or(false);

            from_bot || via_bot || sender_chat_bot
        }
    } else {
        false
    };

    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    (mentioned || replied_to_bot, clean)
}

// ===== Админ-панель: настройки API, модели и промпта =====

/// Текстовое представление текущих настроек для команды /settings.
fn format_settings(mgr: &MemoryManager) -> String {
    let s = mgr.settings_snapshot();
    let mark = |key: &str| if s.is_overridden(key) { "✏️ переопределено в БД" } else { "по умолчанию (.env)" };
    let key_preview = if s.api_key.is_empty() {
        "<пусто / без ключа>".to_string()
    } else {
        mask_key(&s.api_key)
    };
    let prompt_preview: String = if s.system_prompt.chars().count() > 80 {
        let p: String = s.system_prompt.chars().take(80).collect();
        format!("{p}...")
    } else {
        s.system_prompt.clone()
    };
    let def_prompt_preview: String = if s.default_system_prompt.chars().count() > 60 {
        let p: String = s.default_system_prompt.chars().take(60).collect();
        format!("{p}...")
    } else {
        s.default_system_prompt.clone()
    };
    format!(
        "⚙️ Текущие настройки LLM\n\n\
         Модель: {model}\n  → {mark_model}\n\
         API: {base_url}\n  → {mark_api}\n\
         Ключ: {key_preview}\n  → {mark_key}\n\
         Промпт: «{prompt_preview}»\n  → {mark_prompt}\n\n\
         Дефолт из .env: модель «{default_model}», API «{default_base_url}»\n\
         Дефолтный промпт: «{def_prompt_preview}»\n\n\
         Команды изменения:\n\
         /set_model <модель>\n\
         /set_api <base_url>\n\
         /set_key <ключ>\n\
         /set_prompt <текст>\n\
         /reset_setting <model|api|key|prompt|all>",
        model = s.model,
        mark_model = mark(crate::memory::SETTING_MODEL),
        base_url = s.base_url,
        mark_api = mark(crate::memory::SETTING_BASE_URL),
        key_preview = key_preview,
        mark_key = mark(crate::memory::SETTING_API_KEY),
        prompt_preview = prompt_preview,
        mark_prompt = mark(crate::memory::SETTING_SYSTEM_PROMPT),
        default_model = s.default_model,
        default_base_url = s.default_base_url,
        def_prompt_preview = def_prompt_preview,
    )
}

async fn handle_set_model(
    bot: Bot,
    msg: Message,
    value: &str,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Настройки доступны только администратору бота.").await?;
        return Ok(());
    }
    let value = value.trim();
    if value.is_empty() {
        let current = state.mgr.effective_settings().model;
        bot.send_message(
            msg.chat.id,
            format!(
                "🤖 Укажи модель: /set_model <название>\n\
                 Например: /set_model gpt-4o-mini или /set_model deepseek/deepseek-chat\n\n\
                 Текущая модель: {current}\n\
                 Сбросить к значению из .env: /reset_setting model"
            ),
        )
        .await?;
        return Ok(());
    }
    if value.chars().count() > 256 {
        bot.send_message(msg.chat.id, "⚠️ Название модели слишком длинное (максимум 256 символов).").await?;
        return Ok(());
    }
    match state.mgr.set_setting(crate::memory::SETTING_MODEL, value) {
        Ok(()) => {
            tracing::info!(user_id, model = value, "модель обновлена");
            bot.send_message(msg.chat.id, format!("✅ Модель успешно изменена на «{value}».")).await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("⚠️ Не удалось сохранить модель: {e}")).await?;
        }
    }
    Ok(())
}

async fn handle_set_api(
    bot: Bot,
    msg: Message,
    value: &str,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Настройки доступны только администратору бота.").await?;
        return Ok(());
    }
    let value = value.trim();
    if value.is_empty() {
        let current = state.mgr.effective_settings().base_url;
        bot.send_message(
            msg.chat.id,
            format!(
                "🌐 Укажи адрес API: /set_api <base_url>\n\
                 Например: /set_api https://api.openai.com/v1 или /set_api http://localhost:11434/v1\n\n\
                 Текущий адрес API: {current}\n\
                 Сбросить к значению из .env: /reset_setting api"
            ),
        )
        .await?;
        return Ok(());
    }
    if value.chars().count() > 512 {
        bot.send_message(msg.chat.id, "⚠️ Адрес API слишком длинный (максимум 512 символов).").await?;
        return Ok(());
    }
    if !value.starts_with("http://") && !value.starts_with("https://") {
        bot.send_message(msg.chat.id, "⚠️ Адрес API должен начинаться с http:// или https://").await?;
        return Ok(());
    }
    match state.mgr.set_setting(crate::memory::SETTING_BASE_URL, value) {
        Ok(()) => {
            tracing::info!(user_id, base_url = value, "адрес API обновлен");
            bot.send_message(msg.chat.id, format!("✅ Адрес API успешно изменён на «{value}».")).await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("⚠️ Не удалось сохранить адрес API: {e}")).await?;
        }
    }
    Ok(())
}

async fn handle_set_key(
    bot: Bot,
    msg: Message,
    value: &str,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Настройки доступны только администратору бота.").await?;
        return Ok(());
    }
    let value = value.trim();
    if value.is_empty() {
        let current_key = state.mgr.effective_settings().api_key;
        let preview = if current_key.is_empty() {
            "<пусто / без авторизации>".to_string()
        } else {
            mask_key(&current_key)
        };
        bot.send_message(
            msg.chat.id,
            format!(
                "🔑 Укажи API-ключ: /set_key <ключ>\n\
                 Например: /set_key sk-...\n\n\
                 Текущий ключ: {preview}\n\
                 Сбросить к значению из .env: /reset_setting key"
            ),
        )
        .await?;
        return Ok(());
    }
    if value.chars().count() > 512 {
        bot.send_message(msg.chat.id, "⚠️ Ключ слишком длинный (максимум 512 символов).").await?;
        return Ok(());
    }
    match state.mgr.set_setting(crate::memory::SETTING_API_KEY, value) {
        Ok(()) => {
            tracing::info!(user_id, "API-ключ обновлен");
            let shown = mask_key(value);
            bot.send_message(msg.chat.id, format!("✅ API-ключ успешно сохранён: {shown}")).await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("⚠️ Не удалось сохранить API-ключ: {e}")).await?;
        }
    }
    Ok(())
}

async fn handle_set_prompt(
    bot: Bot,
    msg: Message,
    value: &str,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Настройки доступны только администратору бота.").await?;
        return Ok(());
    }
    let value = value.trim();
    if value.is_empty() {
        let current_prompt = state.mgr.effective_system_prompt();
        bot.send_message(
            msg.chat.id,
            format!(
                "📝 Укажи системный промпт: /set_prompt <текст>\n\
                 Например: /set_prompt Ты — эксперт по Rust. Отвечай кратко и емко.\n\n\
                 Текущий системный промпт:\n«{current_prompt}»\n\n\
                 Сбросить к дефолту из .env: /reset_setting prompt"
            ),
        )
        .await?;
        return Ok(());
    }
    if value.chars().count() > 4000 {
        bot.send_message(msg.chat.id, "⚠️ Системный промпт слишком длинный (максимум 4000 символов).").await?;
        return Ok(());
    }
    match state.mgr.set_setting(crate::memory::SETTING_SYSTEM_PROMPT, value) {
        Ok(()) => {
            tracing::info!(user_id, "системный промпт обновлен");
            bot.send_message(msg.chat.id, "✅ Системный промпт успешно сохранён.").await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("⚠️ Не удалось сохранить системный промпт: {e}")).await?;
        }
    }
    Ok(())
}

/// Маскирует ключ для вывода: первые 3 и последние 2 символа, остальное точками.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 6 {
        return "••••".to_string();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}…{tail} (скрыт, {} симв.)", chars.len())
}

async fn handle_reset_setting(
    bot: Bot,
    msg: Message,
    query: &str,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    if !is_admin(&state, user_id) {
        bot.send_message(msg.chat.id, "🔒 Настройки доступны только администратору бота.").await?;
        return Ok(());
    }
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        bot.send_message(
            msg.chat.id,
            "Укажи, какую настройку сбросить: /reset_setting <model|api|key|prompt|all>\n\
             • model — сбросить модель к значению из .env\n\
             • api — сбросить адрес API к значению из .env\n\
             • key — сбросить API-ключ к значению из .env\n\
             • prompt — сбросить системный промпт к значению по умолчанию\n\
             • all — сбросить все переопределённые настройки\n\n\
             Текущие настройки: /settings",
        )
        .await?;
        return Ok(());
    }
    if query == "all" || query == "всё" || query == "все" {
        let keys = [
            crate::memory::SETTING_MODEL,
            crate::memory::SETTING_BASE_URL,
            crate::memory::SETTING_API_KEY,
            crate::memory::SETTING_SYSTEM_PROMPT,
        ];
        let mut count = 0;
        for key in keys {
            if let Ok(true) = state.mgr.reset_setting(key) {
                count += 1;
            }
        }
        if count > 0 {
            tracing::info!(user_id, count, "сброшены все настройки");
            bot.send_message(msg.chat.id, "♻️ Все настройки сброшены к значениям по умолчанию из .env.").await?;
        } else {
            bot.send_message(msg.chat.id, "Настройки и так используют значения по умолчанию из .env.").await?;
        }
        return Ok(());
    }
    let key = match query.as_str() {
        "model" | "модель" => crate::memory::SETTING_MODEL,
        "api" | "url" | "base_url" | "адрес" => crate::memory::SETTING_BASE_URL,
        "key" | "ключ" => crate::memory::SETTING_API_KEY,
        "prompt" | "system_prompt" | "промпт" => crate::memory::SETTING_SYSTEM_PROMPT,
        _ => {
            bot.send_message(
                msg.chat.id,
                "⚠️ Неизвестная настройка. Доступно: model, api, key, prompt, all.",
            )
            .await?;
            return Ok(());
        }
    };
    let label = setting_label(key);
    match state.mgr.reset_setting(key) {
        Ok(true) => {
            tracing::info!(user_id, key, "настройка сброшена");
            bot.send_message(msg.chat.id, format!("♻️ Настройка «{label}» сброшена к значению из .env.")).await?;
        }
        Ok(false) => {
            bot.send_message(msg.chat.id, format!("Настройка «{label}» и так использует значение из .env.")).await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("⚠️ Не удалось сбросить: {e}")).await?;
        }
    }
    Ok(())
}

fn setting_label(key: &str) -> &'static str {
    match key {
        crate::memory::SETTING_MODEL => "модель",
        crate::memory::SETTING_BASE_URL => "API",
        crate::memory::SETTING_API_KEY => "ключ",
        crate::memory::SETTING_SYSTEM_PROMPT => "системный промпт",
        _ => "настройка",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parsing_and_redirects() {
        assert!(Command::parse("/start", "bot").is_ok());
        assert!(Command::parse("/settings", "bot").is_ok());
        assert!(Command::parse("/set_model gpt-4o", "bot").is_ok());
        assert!(Command::parse("/guide_add Название", "bot").is_ok());
        assert!(Command::parse("/guide_del 1", "bot").is_ok());

        assert_eq!(
            pm_redirect_message(Some("my_bot")),
            "ℹ️ Команды и настройки бота доступны только в личных сообщениях: @my_bot"
        );
        assert_eq!(
            pm_redirect_message(None),
            "ℹ️ Команды и настройки бота доступны только в личных сообщениях с ботом."
        );
    }

    #[test]
    fn mask_key_hides_middle() {
        assert_eq!(mask_key("abc"), "••••");
        assert_eq!(mask_key("abcdef"), "••••");
        let m = mask_key("sk-abcd-1234-xyz");
        assert!(m.starts_with("sk-"));
        assert!(m.contains("скрыт"));
        assert!(!m.contains("abcd"));
    }
}

