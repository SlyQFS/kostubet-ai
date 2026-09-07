mod bot;
mod config;
mod knowledge;
mod llm;
mod storage;
mod utils;

use std::sync::Arc;
use std::time::Duration;

use tracing::error;
use tracing_subscriber::EnvFilter;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use bot::handlers_admin::Command;
use bot::{router, App, RateLimiter};
use config::Config;
use knowledge::loader::KnowledgeBase;
use knowledge::matcher::Matcher;
use llm::client::LlmClient;
use llm::state::ModelState;
use storage::Store;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cfg = match Config::from_env() {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            error!("Конфигурация: {e}");
            std::process::exit(1);
        }
    };

    let store = match Store::connect(&cfg.db_path).await {
        Ok(store) => Arc::new(store),
        Err(e) => {
            error!("База данных {}: {e}", cfg.db_path);
            std::process::exit(1);
        }
    };

    let kb = match KnowledgeBase::load(&cfg.knowledge_dir) {
        Ok(kb) => kb,
        Err(e) => {
            error!("База знаний: {e}");
            std::process::exit(1);
        }
    };
    let cards = kb.cards.len();
    let cache_dir = cfg.models_cache_dir.clone();
    let matcher = Arc::new(
        match tokio::task::spawn_blocking(move || Matcher::new(kb, &cache_dir)).await {
            Ok(matcher) => matcher,
            Err(e) => {
                error!("Поток эмбеддера базы знаний: {e}");
                std::process::exit(1);
            }
        },
    );
    tracing::info!(
        "база знаний: {cards} карточек, семантика: {}",
        matcher.has_semantic()
    );

    let bot = teloxide::Bot::new(cfg.bot_token.clone());
    let me = match bot.get_me().await {
        Ok(me) => me,
        Err(e) => {
            error!("Не удалось получить профиль бота: {e}");
            std::process::exit(1);
        }
    };

    let limiter = RateLimiter::new(
        cfg.rate_limit_requests,
        Duration::from_secs(cfg.rate_limit_window_secs),
    );

    let app = Arc::new(App {
        cfg: cfg.clone(),
        store,
        llm: LlmClient::new(&cfg.llm_base_url, &cfg.llm_api_key),
        models: ModelState::new(cfg.models.clone()),
        matcher,
        queue: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_requests)),
        last_request: Arc::new(tokio::sync::Mutex::new(std::time::Instant::now())),
        limiter,
        bot_id: me.id.0,
        bot_username: me.username.clone().unwrap_or_default(),
    });

    if let Err(e) = bot.set_my_commands(Command::bot_commands()).await {
        error!("Не удалось зарегистрировать меню команд: {e}");
    }

    tracing::info!(
        "KostubetAI 2.0 запущен: @{}, моделей: {} (активна {})",
        app.bot_username,
        app.models.list().len(),
        app.models.current()
    );

    teloxide::prelude::Dispatcher::builder(bot, router::build_handler())
        .dependencies(teloxide::prelude::dptree::deps![app])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
