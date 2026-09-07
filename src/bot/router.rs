//! Маршрутизация обновлений: сообщения (команды + чат) и callback-запросы.

use dptree::entry;
use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;

use super::handlers_admin::{self, Command};
use super::handlers_chat;

pub fn build_handler() -> UpdateHandler<teloxide::RequestError> {
    dptree::entry()
        .branch(
            Update::filter_message()
                .branch(entry().filter_command::<Command>().endpoint(handlers_admin::command))
                .branch(entry().endpoint(handlers_chat::message)),
        )
        .branch(Update::filter_callback_query().endpoint(handlers_admin::callback))
}
