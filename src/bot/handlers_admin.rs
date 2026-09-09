//! Админка в ЛС: команды и inline-переключение моделей.

use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use teloxide::utils::command::BotCommands;

use super::App;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Команды админки")]
pub enum Command {
    #[command(description = "запуск: статус и краткая помощь")]
    Start,
    #[command(description = "помощь")]
    Help,
    #[command(description = "переключить модель (inline-кнопки)")]
    Model,
    #[command(description = "переключить стриминг ответов (вкл/выкл)")]
    Stream,
}

/// Точка входа команд: работает только в ЛС, чужие команды в группах
/// молча игнорируются (см. handlers_chat).
pub async fn command(bot: Bot, msg: Message, cmd: Command, app: Arc<App>) -> ResponseResult<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }
    let Some(user) = msg.from else {
        return Ok(());
    };
    if !app.cfg.is_admin(user.id.0 as i64) {
        bot.send_message(msg.chat.id, stub_text()).await?;
        return Ok(());
    }

    match cmd {
        Command::Start => {
            bot.send_message(msg.chat.id, start_text(&app)).await?;
        }
        Command::Help => {
            bot.send_message(msg.chat.id, help_text()).await?;
        }
        Command::Model => {
            let keyboard = models_keyboard(&app, None);
            let text = models_text(&app);
            match bot
                .send_message(msg.chat.id, text)
                .reply_markup(keyboard)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("не удалось отправить меню моделей: {e}");
                    bot.send_message(msg.chat.id, "Не удалось открыть меню моделей.").await?;
                }
            }
        }
        Command::Stream => {
            let enabled = app.is_streaming();
            let keyboard = stream_keyboard(enabled);
            let text = stream_text(enabled);
            bot.send_message(msg.chat.id, text)
                .reply_markup(keyboard)
                .await
                .ok();
        }
    }
    Ok(())
}

/// Callback от inline-кнопок `/model` и `/stream`.
pub async fn callback(bot: Bot, q: CallbackQuery, app: Arc<App>) -> ResponseResult<()> {
    let from = &q.from;
    if !app.cfg.is_admin(from.id.0 as i64) {
        bot.answer_callback_query(q.id.clone())
            .text("Только для админов.")
            .await?;
        return Ok(());
    }

    let Some(data) = q.data.as_deref() else {
        return Ok(());
    };

    if let Some(index_str) = data.strip_prefix("model:") {
        let result = index_str
            .parse::<usize>()
            .ok()
            .and_then(|i| app.models.set_index(i).ok());

        match result {
            Some(model) => {
                tracing::info!("админ {} переключил модель на {model}", from.id);
                if let Some(message) = q.message.as_ref() {
                    let keyboard = models_keyboard(&app, Some(app.models.active_index()));
                    bot.edit_message_text(
                        message.chat().id,
                        message.id(),
                        models_text(&app),
                    )
                    .reply_markup(keyboard)
                    .await
                    .ok();
                }
                bot.answer_callback_query(q.id.clone())
                    .text(format!("Модель: {model}"))
                    .await?;
            }
            None => {
                bot.answer_callback_query(q.id.clone())
                    .text("Такой модели нет в списке.")
                    .await?;
            }
        }
    } else if data == "stream:toggle" {
        let enabled = app.toggle_streaming();
        tracing::info!("админ {} переключил стриминг: {enabled}", from.id);
        if let Some(message) = q.message.as_ref() {
            bot.edit_message_text(
                message.chat().id,
                message.id(),
                stream_text(enabled),
            )
            .reply_markup(stream_keyboard(enabled))
            .await
            .ok();
        }
        let hint = if enabled { "Стриминг включён" } else { "Стриминг выключен" };
        bot.answer_callback_query(q.id.clone()).text(hint).await?;
    }
    Ok(())
}

fn models_text(app: &App) -> String {
    format!(
        "Активная модель: {}\n\nВыбери модель — переключение применяется сразу ко всем чатам.",
        app.models.current()
    )
}

fn models_keyboard(app: &App, active: Option<usize>) -> InlineKeyboardMarkup {
    let mut rows = Vec::new();
    for (i, model) in app.models.list().iter().enumerate() {
        let label = if Some(i) == active || (active.is_none() && i == app.models.active_index()) {
            format!("✔ {model}")
        } else {
            model.clone()
        };
        rows.push(vec![InlineKeyboardButton::callback(label, format!("model:{i}"))]);
    }
    InlineKeyboardMarkup::new(rows)
}

fn stream_text(enabled: bool) -> String {
    let status = if enabled {
        "ВКЛЮЧЁН (SSE + живой вывод текста в чат)"
    } else {
        "ВЫКЛЮЧЕН (отправка ответа целиком после генерации)"
    };
    format!("Режим стриминга ответов: {status}\n\nНажми кнопку ниже, чтобы переключить режим на лету:")
}

fn stream_keyboard(enabled: bool) -> InlineKeyboardMarkup {
    let label = if enabled {
        "🟢 Стриминг: ВКЛ (нажми, чтобы выключить)"
    } else {
        "🔴 Стриминг: ВЫКЛ (нажми, чтобы включить)"
    };
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(label, "stream:toggle")]])
}

fn stub_text() -> String {
    "Этот бот отвечает только в группах. Управление — у администраторов.".to_string()
}

fn start_text(app: &App) -> String {
    let stream_status = if app.is_streaming() { "включен" } else { "выключен" };
    format!(
        "KostubetAI 2.0\nАктивная модель: {}\nСтриминг: {}\n\nКоманды:\n/model — сменить модель\n/stream — переключить стриминг ответов\n/help — помощь\n\nВ группах бот отвечает на @{} и reply к его сообщениям.",
        app.models.current(),
        stream_status,
        app.bot_username
    )
}

fn help_text() -> String {
    "Память: бот держит краткий контекст диалога и мета-блоки участников (цель, текущий шаг).\n\
     База знаний подключается автоматически по смыслу запроса.\n\
     /model — переключение модели (действует сразу на все чаты).\n\
     /stream — управление стримингом ответов (вкл/выкл на лету)."
        .to_string()
}
