mod config;
mod db;
mod handlers;
mod llm;
mod memory;
mod text;

use std::sync::{Arc, Mutex};

use teloxide::prelude::*;

use crate::config::Config;
use crate::db::MemoryStore;
use crate::llm::LlmClient;
use crate::memory::MemoryManager;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Ошибка конфигурации: {e}");
            std::process::exit(1);
        }
    };

    let mut store = match MemoryStore::open(&cfg.database_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Не удалось открыть базу данных {}: {e}", cfg.database_path);
            std::process::exit(1);
        }
    };
    // Если база разрослась с прошлого запуска — сразу приводим её к лимиту.
    if store.db_disk_size() > cfg.memory_limit_bytes * 95 / 100 {
        match store.enforce_limit(cfg.memory_limit_bytes) {
            Ok(stage) => tracing::info!(stage, "база уменьшена при старте"),
            Err(e) => tracing::warn!("не удалось очистить базу при старте: {e}"),
        }
    }

    let manager = MemoryManager::new(
        Arc::new(Mutex::new(store)),
        LlmClient::new(cfg.llm_base_url.clone(), cfg.llm_api_key.clone(), cfg.llm_model.clone()),
        cfg.clone(),
    );

    let bot = Bot::new(cfg.bot_token.clone());
    let me = match bot.get_me().await {
        Ok(me) => me,
        Err(e) => {
            eprintln!("Не удалось подключиться к Telegram (проверьте TELEGRAM_BOT_TOKEN): {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = bot.set_my_commands(handlers::bot_commands()).await {
        tracing::warn!("не удалось зарегистрировать меню команд: {e}");
    }

    let username = me.username.as_deref().map(str::to_owned);
    tracing::info!(
        "KostubetAI запущен как @{} | модель: {} | память: {} токенов/пользователя, лимит базы: {}",
        username.as_deref().unwrap_or("?"),
        cfg.llm_model,
        cfg.user_memory_tokens,
        text::fmt_bytes(cfg.memory_limit_bytes),
    );

    let state = Arc::new(handlers::AppState {
        mgr: manager,
        bot_id: me.id,
        bot_username: username,
        cfg: cfg.clone(),
        http: reqwest::Client::new(),
    });

    let cmd_state = state.clone();
    let msg_state = state.clone();

    let handler = Update::filter_message()
        .filter_command::<handlers::Command>()
        .endpoint(move |bot: Bot, msg: Message, cmd: handlers::Command| {
            let state = cmd_state.clone();
            async move { handlers::cmd_handler(bot, msg, cmd, state).await }
        })
        .branch(
            Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
                let state = msg_state.clone();
                async move { handlers::on_message(bot, msg, state).await }
            }),
        );

    let mut dispatcher = Dispatcher::builder(bot, handler).build();
    tokio::select! {
        _ = dispatcher.dispatch() => {}
        _ = shutdown_signal() => tracing::info!("получен сигнал остановки — завершаю работу"),
    }
}

/// Ждёт SIGINT (Ctrl-C) или SIGTERM (`docker stop`, systemd), чтобы процесс
/// завершался чисто, а не убивался сигналом KILL по таймауту.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("не удалось подписаться на SIGTERM");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await
    }
}
