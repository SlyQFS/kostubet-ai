use std::sync::Arc;
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{ChatAction, ChatId, Document, ReplyParameters, User, UserId};
use teloxide::utils::command::BotCommands;

use crate::config::Config;
use crate::memory::MemoryManager;
use crate::text::{fmt_bytes, split_telegram};

/// Максимальный размер текстового файла-гайда.
const MAX_GUIDE_FILE_BYTES: u64 = 5 * 1024 * 1024;

const HELP_TEXT: &str = "\
Привет! Я KostubetAI — ИИ-собеседник с памятью 🧠

Что я умею:
• Помню нашу переписку — до 5000 токенов твоих последних сообщений, ничего лишнего.
• Опираюсь на загруженные гайды, когда они подходят к твоему вопросу.

Команды:
/reset — очистить мою память о тебе
/memory — статистика памяти
/guides — список гайдов
/guide_add <название> — добавить гайд (ответь этой командой на сообщение с текстом)
/guide_del <название или id> — удалить гайд

Гайд можно также загрузить файлом: просто пришли мне текстовый документ (.txt, .md, ...) с названием в подписи.";

pub struct AppState {
    pub mgr: MemoryManager,
    pub bot_id: UserId,
    pub bot_username: Option<String>,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Команды KostubetAI:")]
pub enum Command {
    #[command(description = "приветствие и справка")]
    Start,
    #[command(description = "справка")]
    Help,
    #[command(description = "очистить мою память о тебе")]
    Reset,
    #[command(description = "статистика памяти")]
    Memory,
    #[command(description = "список гайдов")]
    Guides,
}

/// Список команд для Telegram-меню (включая те, что разбираются вручную).
pub fn bot_commands() -> Vec<teloxide::types::BotCommand> {
    use teloxide::types::BotCommand;
    vec![
        BotCommand::new("start", "приветствие и справка"),
        BotCommand::new("help", "справка"),
        BotCommand::new("reset", "очистить мою память о тебе"),
        BotCommand::new("memory", "статистика памяти"),
        BotCommand::new("guides", "список гайдов"),
        BotCommand::new("guide_add", "добавить гайд (ответом на сообщение)"),
        BotCommand::new("guide_del", "удалить гайд"),
    ]
}

pub async fn cmd_handler(bot: Bot, msg: Message, cmd: Command, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    let text = match cmd {
        Command::Start | Command::Help => HELP_TEXT.to_string(),
        Command::Reset => match state.mgr.reset(user_id) {
            Ok(()) => "🧠 Готово: я забыл все твои сообщения. Гайды не тронуты — их список: /guides".to_string(),
            Err(e) => {
                tracing::error!("ошибка сброса памяти: {e}");
                "⚠️ Не удалось очистить память.".to_string()
            }
        },
        Command::Memory => {
            let snap = state.mgr.user_snapshot(user_id);
            let budget = state.mgr.user_memory_tokens();
            let (size, total_msgs, users, guides) = state.mgr.global_stats();
            format!(
                "👤 Твоя память: {} сообщений, ~{} из {} токенов ({:.0}%)\n\
                 💾 База всего: {} из {} ({:.1}%), сообщений: {}, пользователей: {}, гайдов: {}\n\n\
                 /summary не нужен — я помню твои сообщения дословно, без пересказов.",
                snap.message_count,
                snap.est_tokens,
                budget,
                snap.est_tokens as f64 / budget as f64 * 100.0,
                fmt_bytes(size),
                fmt_bytes(state.cfg.memory_limit_bytes),
                size as f64 / state.cfg.memory_limit_bytes as f64 * 100.0,
                total_msgs,
                users,
                guides,
            )
        }
        Command::Guides => {
            let guides = state.mgr.guides();
            if guides.is_empty() {
                "📚 Гайдов пока нет.\n\nЗагрузить: пришли текстовый файл или ответь /guide_add <название> на сообщение с текстом.".to_string()
            } else {
                let mut out = format!("📚 Гайдов: {}\n\n", guides.len());
                for g in guides {
                    out.push_str(&format!("{}. {} (~{} симв.)\n", g.id, g.title, g.chars));
                }
                out.push_str("\nУдалить: /guide_del <название или id>");
                out
            }
        }
    };
    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

pub async fn on_message(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    // Документы — кандидаты на загрузку как гайд.
    if let Some(doc) = msg.document().cloned() {
        return handle_document(bot, msg, &doc, state).await;
    }

    let Some(text) = msg.text().map(ToOwned::to_owned) else {
        return Ok(());
    };

    // Команды с аргументами разбираем вручную, чтобы названия гайдов могли содержать пробелы.
    if let Some(title) = manual_command_args(&text, "/guide_add") {
        return handle_guide_add(bot, msg, title, state).await;
    }
    if let Some(query) = manual_command_args(&text, "/guide_del") {
        return handle_guide_del(bot, msg, query, state).await;
    }

    let (should_reply, clean) = should_reply(&msg, &state, &text);
    if !should_reply {
        return Ok(());
    }
    let user_text = if clean.is_empty() { "Привет!".to_string() } else { clean };
    let Some(user_id) = msg.from.as_ref().map(|u| u.id.0 as i64) else {
        return Ok(());
    };

    let typing = spawn_typing(bot.clone(), msg.chat.id);
    let answer = state.mgr.reply(user_id, &user_text).await;
    typing.abort();

    match answer {
        Ok(a) if !a.trim().is_empty() => send_chunks(&bot, &msg, a.trim()).await?,
        Ok(_) => tracing::warn!(user_id, "модель вернула пустой ответ"),
        Err(e) => {
            tracing::error!("ошибка генерации ответа для {user_id}: {e}");
            let _ = bot
                .send_message(
                    msg.chat.id,
                    "⚠️ Не получилось связаться с моделью. Попробуй ещё раз чуть позже.",
                )
                .reply_parameters(ReplyParameters::new(msg.id))
                .await;
        }
    }
    Ok(())
}

/// Разбирает `/command@bot аргументы` без аргументов-полей из BotCommands,
/// чтобы аргумент мог содержать пробелы. Возвращает None, если это другая команда.
fn manual_command_args<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(name)?;
    let rest = if let Some(tag) = rest.strip_prefix('@') {
        let end = tag.find(char::is_whitespace).unwrap_or(tag.len());
        &tag[end..]
    } else {
        rest
    };
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

fn is_admin(state: &AppState, user_id: i64) -> bool {
    state.cfg.admins.is_empty() || state.cfg.admins.contains(&user_id)
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

    // В группах реагируем только на документы, присланные с упоминанием бота.
    let caption = msg.caption().map(ToOwned::to_owned).unwrap_or_default();
    let (should_reply, _) = should_reply(&msg, &state, &caption);
    if !should_reply {
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

/// В приватном чате отвечаем всегда; в группах — только при упоминании @бота
/// или когда пользователь отвечает на сообщение бота.
fn should_reply(msg: &Message, state: &AppState, text: &str) -> (bool, String) {
    if msg.chat.is_private() {
        return (true, text.trim().to_string());
    }
    let mut mentioned = false;
    let mut clean = text.to_string();
    if let Some(username) = &state.bot_username {
        let tag = format!("@{username}");
        if clean.contains(&tag) {
            mentioned = true;
            clean = clean.replace(&tag, " ");
        }
    }
    let replied_to_bot = msg
        .reply_to_message()
        .and_then(|m| m.from.as_ref())
        .map(|u: &User| u.id == state.bot_id)
        .unwrap_or(false);
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    (mentioned || replied_to_bot, clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_command_args_parsing() {
        assert_eq!(manual_command_args("/guide_add Моё название", "/guide_add"), Some("Моё название"));
        assert_eq!(manual_command_args("/guide_add@my_bot Название", "/guide_add"), Some("Название"));
        assert_eq!(manual_command_args("/guide_add", "/guide_add"), Some(""));
        assert_eq!(manual_command_args("/guide_del rust", "/guide_del"), Some("rust"));
        assert_eq!(manual_command_args("/guide_addxxx", "/guide_add"), None);
        assert_eq!(manual_command_args("привет", "/guide_add"), None);
    }
}
