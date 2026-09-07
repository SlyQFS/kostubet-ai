-- KostubetAI 2.0: начальная схема.

-- История диалога по чатам/топикам (скользящее окно, system-сообщения не хранятся).
CREATE TABLE IF NOT EXISTS messages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id      INTEGER NOT NULL,
    thread_id    INTEGER NOT NULL DEFAULT 0,
    user_id      INTEGER NOT NULL,
    display_name TEXT    NOT NULL DEFAULT '',
    role         TEXT    NOT NULL,           -- 'user' | 'assistant'
    content      TEXT    NOT NULL,
    created_at   INTEGER NOT NULL            -- unix seconds
);

CREATE INDEX IF NOT EXISTS idx_messages_scope ON messages (chat_id, thread_id, id);

-- Мета-блок собеседника: цель и текущий прогресс, поддерживается моделью через <user:...>.
CREATE TABLE IF NOT EXISTS user_states (
    user_id      INTEGER PRIMARY KEY,
    display_name TEXT    NOT NULL DEFAULT '',
    goal         TEXT    NOT NULL DEFAULT '',
    current      TEXT    NOT NULL DEFAULT '',
    updated_at   INTEGER NOT NULL
);
