use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::text::{meaningful_words, split_chunks};

/// Сообщение, сохранённое в памяти чата.
#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    pub display_name: Option<String>,
}

/// Сведений о гайде для команды /guides.
#[derive(Clone, Debug)]
pub struct GuideInfo {
    pub id: i64,
    pub title: String,
    pub chars: usize,
}

#[derive(Debug)]
pub enum AddGuideError {
    Duplicate,
    Empty,
    Db(rusqlite::Error),
}

impl std::fmt::Display for AddGuideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddGuideError::Duplicate => write!(f, "гайд с таким названием уже существует"),
            AddGuideError::Empty => write!(f, "текст гайда пуст"),
            AddGuideError::Db(e) => write!(f, "ошибка базы данных: {e}"),
        }
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    user_id       INTEGER PRIMARY KEY,
    first_seen    INTEGER NOT NULL,
    last_activity INTEGER NOT NULL,
    display_name  TEXT    NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    chat_id    INTEGER NOT NULL DEFAULT 0,
    role       TEXT    NOT NULL,
    content    TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_user ON messages(user_id, id);

CREATE TABLE IF NOT EXISTS guides (
    guide_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    title      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    added_by   INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS guide_chunks (
    chunk_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    guide_id    INTEGER NOT NULL REFERENCES guides(guide_id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    content     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chunks_guide ON guide_chunks(guide_id);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const CHUNK_TARGET_CHARS: usize = 1200;
const CHUNK_HARD_LIMIT_CHARS: usize = 2000;

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Грубая оценка числа токенов (без словаря токенизатора): ~3 символа на токен.
/// Русский и английский текст в среднем укладывается в эту оценку с запасом.
fn est_chars_to_tokens(chars: usize) -> usize {
    chars.div_ceil(3)
}

/// Проверяет, существует ли колонка в таблице (для миграций).
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let Ok(mut stmt) = conn.prepare(&sql) else { return false };
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    names.iter().any(|c| c == column)
}

/// Миграции схемы для существующих баз данных.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "messages", "chat_id") {
        conn.execute("ALTER TABLE messages ADD COLUMN chat_id INTEGER NOT NULL DEFAULT 0", [])?;
    }
    if !column_exists(conn, "users", "display_name") {
        conn.execute("ALTER TABLE users ADD COLUMN display_name TEXT NOT NULL DEFAULT ''", [])?;
    }
    conn.execute("CREATE INDEX IF NOT EXISTS idx_messages_chat ON messages(chat_id, id)", [])?;
    Ok(())
}

/// Хранилище: история переписки отдельно по каждому пользователю (с лимитом
/// токенов на пользователя) и глобальная база гайдов. Общий размер базы
/// ограничен и контролируется функцией `enforce_limit`.
pub struct MemoryStore {
    conn: Connection,
    db_path: PathBuf,
}

impl MemoryStore {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let db_path = Path::new(path);
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // journal_mode возвращает строку с результатом, поэтому через query_row
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL;", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn, db_path: db_path.to_path_buf() })
    }

    // ===== Полу-разделяемая история чатов =====

    /// Сохраняет сообщение в общий контекст чата. `display_name` обновляется
    /// в таблице users только если непусто (чтобы ассистент не затирал имя).
    pub fn add_message(
        &self,
        user_id: i64,
        chat_id: i64,
        display_name: &str,
        role: &str,
        content: &str,
    ) -> rusqlite::Result<()> {
        let now = unix_now();
        self.conn.execute(
            "INSERT INTO users(user_id, first_seen, last_activity, display_name)
             VALUES(?1, ?2, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET
                last_activity = ?2,
                display_name = COALESCE(NULLIF(?3, ''), users.display_name)",
            params![user_id, now, display_name],
        )?;
        self.conn.execute(
            "INSERT INTO messages(user_id, chat_id, role, content, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![user_id, chat_id, role, content, now],
        )?;
        Ok(())
    }

    /// Последние сообщения всех участников чата, укладывающиеся в бюджет
    /// токенов (от свежих к старым). display_name берётся из таблицы users.
    pub fn chat_messages_within_tokens(
        &self,
        chat_id: i64,
        token_budget: usize,
    ) -> rusqlite::Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.role, m.content, u.display_name
             FROM messages m
             LEFT JOIN users u ON u.user_id = m.user_id
             WHERE m.chat_id = ?1
             ORDER BY m.id DESC",
        )?;
        let rows = stmt.query_map(params![chat_id], |r| {
            Ok(StoredMessage {
                role: r.get(0)?,
                content: r.get(1)?,
                display_name: r.get(2)?,
            })
        })?;
        let char_budget = token_budget.saturating_mul(3);
        let mut kept = Vec::new();
        let mut acc = 0usize;
        for row in rows {
            let msg = row?;
            if acc >= char_budget {
                break;
            }
            acc += msg.content.chars().count();
            kept.push(msg);
        }
        kept.reverse();
        Ok(kept)
    }

    /// Число уникальных участников (role='user') в чате.
    pub fn chat_participant_count(&self, chat_id: i64) -> rusqlite::Result<usize> {
        self.conn
            .query_row(
                "SELECT COUNT(DISTINCT user_id) FROM messages WHERE chat_id = ?1 AND role = 'user'",
                params![chat_id],
                |r| Ok(r.get::<_, i64>(0)? as usize),
            )
            .or(Ok(0))
    }

    /// Удаляет сообщения чата, не попадающие в бюджет токенов.
    /// Возвращает число удалённых строк.
    pub fn trim_chat_history(&self, chat_id: i64, token_budget: usize) -> rusqlite::Result<usize> {
        let char_budget = token_budget.saturating_mul(3) as i64;
        self.conn.execute(
            "DELETE FROM messages WHERE chat_id = ?1 AND id IN (
                SELECT id FROM (
                    SELECT id, length(content) AS len,
                           SUM(length(content)) OVER (
                               PARTITION BY chat_id ORDER BY id DESC
                               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                           ) AS running
                    FROM messages WHERE chat_id = ?1
                ) WHERE running - len >= ?2
            )",
            params![chat_id, char_budget],
        )
    }

    /// (число сообщений, оценка занятых токенов, число участников) по чату.
    pub fn chat_stats(&self, chat_id: i64) -> rusqlite::Result<(i64, usize, usize)> {
        let (msg_count, total_chars): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(content)), 0)
                 FROM messages WHERE chat_id = ?1",
                params![chat_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map(|o| o.unwrap_or((0, 0)))?;
        let participants = self.chat_participant_count(chat_id)?;
        Ok((msg_count, est_chars_to_tokens(total_chars as usize), participants))
    }

    pub fn reset_chat(&self, chat_id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM messages WHERE chat_id = ?1", params![chat_id])?;
        Ok(())
    }

    fn trim_all_chats(&self, char_budget: i64) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM messages WHERE id IN (
                SELECT id FROM (
                    SELECT id, length(content) AS len,
                           SUM(length(content)) OVER (
                               PARTITION BY chat_id ORDER BY id DESC
                               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                           ) AS running
                    FROM messages
                ) WHERE running - len >= ?1
            )",
            params![char_budget],
        )
    }

    // ===== Гайды =====

    pub fn add_guide(&mut self, title: &str, added_by: i64, content: &str) -> Result<usize, AddGuideError> {
        let title = title.trim();
        if title.is_empty() || content.trim().is_empty() {
            return Err(AddGuideError::Empty);
        }
        let exists: Option<i64> = self
            .conn
            .query_row("SELECT guide_id FROM guides WHERE title = ?1", params![title], |r| r.get(0))
            .optional()
            .map_err(AddGuideError::Db)?;
        if exists.is_some() {
            return Err(AddGuideError::Duplicate);
        }

        let chunks = split_chunks(content, CHUNK_TARGET_CHARS, CHUNK_HARD_LIMIT_CHARS);
        let tx = self.conn.transaction().map_err(AddGuideError::Db)?;
        tx.execute(
            "INSERT INTO guides(title, added_by, created_at) VALUES(?1, ?2, ?3)",
            params![title, added_by, unix_now()],
        )
        .map_err(AddGuideError::Db)?;
        let guide_id = tx.last_insert_rowid();
        for (index, chunk) in chunks.iter().enumerate() {
            tx.execute(
                "INSERT INTO guide_chunks(guide_id, chunk_index, content) VALUES(?1, ?2, ?3)",
                params![guide_id, index as i64, chunk],
            )
            .map_err(AddGuideError::Db)?;
        }
        tx.commit().map_err(AddGuideError::Db)?;
        Ok(chunks.len())
    }

    pub fn guides(&self) -> rusqlite::Result<Vec<GuideInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.guide_id, g.title, COALESCE(SUM(length(c.content)), 0)
             FROM guides g LEFT JOIN guide_chunks c ON c.guide_id = g.guide_id
             GROUP BY g.guide_id ORDER BY g.guide_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(GuideInfo { id: r.get(0)?, title: r.get(1)?, chars: r.get::<_, i64>(2)? as usize })
        })?;
        rows.collect()
    }

    pub fn guides_count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM guides", [], |r| r.get(0))
    }

    /// Поиск гайда для удаления: точный id, точное название или уникальный префикс.
    pub fn find_guide(&self, query: &str) -> rusqlite::Result<Option<i64>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(None);
        }
        if let Ok(id) = query.parse::<i64>() {
            let found: Option<i64> = self
                .conn
                .query_row("SELECT guide_id FROM guides WHERE guide_id = ?1", params![id], |r| r.get(0))
                .optional()?;
            if found.is_some() {
                return Ok(found);
            }
        }
        let exact: Option<i64> = self
            .conn
            .query_row("SELECT guide_id FROM guides WHERE title = ?1", params![query], |r| r.get(0))
            .optional()?;
        if exact.is_some() {
            return Ok(exact);
        }
        self.conn
            .query_row(
                "SELECT guide_id FROM guides WHERE title LIKE ?1 || '%' ORDER BY length(title) LIMIT 1",
                params![query],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn delete_guide(&self, guide_id: i64) -> rusqlite::Result<bool> {
        let deleted = self.conn.execute("DELETE FROM guides WHERE guide_id = ?1", params![guide_id])?;
        Ok(deleted > 0)
    }

    /// Релевантные вопросу фрагменты гайдов (заголовок + текст), отобранные
    /// по совпадению значимых слов и уложенные в бюджет символов.
    pub fn search_guide_chunks(
        &self,
        query_words: &HashSet<String>,
        budget_chars: usize,
    ) -> rusqlite::Result<Vec<(String, String)>> {
        if query_words.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT g.title, c.chunk_index, c.content
             FROM guide_chunks c JOIN guides g ON g.guide_id = c.guide_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
        })?;

        let mut scored: Vec<(usize, i64, String, String)> = Vec::new();
        for row in rows {
            let (title, index, chunk) = row?;
            let title_score = meaningful_words(&title).intersection(query_words).count() * 2;
            let chunk_score = meaningful_words(&chunk).intersection(query_words).count();
            let total_score = title_score + chunk_score;
            if total_score > 0 {
                scored.push((total_score, index, title, chunk));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        let mut selected: Vec<(String, String)> = Vec::new();
        let mut used = 0usize;
        for (_, _, title, chunk) in scored {
            let len = chunk.chars().count();
            if used + len > budget_chars {
                continue;
            }
            used += len;
            selected.push((title, chunk));
            if used >= budget_chars {
                break;
            }
        }
        Ok(selected)
    }

    // ===== Настройки (переопределяемые из админ-панели) =====

    /// Возвращает значение настройки по ключу, если она задана.
    pub fn get_setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
    }

    /// Записывает настройку (вставляет или обновляет существующую).
    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Удаляет настройку по ключу. Возвращает true, если она существовала.
    pub fn delete_setting(&self, key: &str) -> rusqlite::Result<bool> {
        let deleted = self.conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        Ok(deleted > 0)
    }

    /// Возвращает список всех сохранённых ключей (для команды /settings).
    pub fn all_settings(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT key FROM settings ORDER BY key")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    // ===== Общая статистика и лимит =====

    pub fn total_messages(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
    }

    pub fn total_users(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
    }

    /// Фактический размер на диске: файл базы плюс WAL-журнал.
    /// Транзиентный -shm файл не учитывается: он пересоздаётся при каждом
    /// запуске и удаляется при закрытии базы.
    pub fn db_disk_size(&self) -> u64 {
        let mut total = 0u64;
        let mut paths = vec![self.db_path.clone()];
        let mut wal = self.db_path.clone().into_os_string();
        wal.push("-wal");
        paths.push(PathBuf::from(wal));
        for p in paths {
            if let Ok(meta) = std::fs::metadata(&p) {
                total += meta.len();
            }
        }
        total
    }

    fn vacuum(&self) {
        if let Err(e) = self.conn.execute("VACUUM", []) {
            tracing::warn!("VACUUM не удался: {e}");
        }
        // wal_checkpoint возвращает строку — читаем её и игнорируем
        let _ = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |_| Ok(()));
    }

    /// Следит за общим лимитом размера базы. Возвращает номер этапа, на котором
    /// удалось уложиться (0 — лимит и так соблюдён). Этапы от бережных к жёстким:
    /// 1) урезать историю всех пользователей до ~1000 токенов;
    /// 2) урезать историю всех до ~100 токенов;
    /// 3) удалить истории всех, кроме 500 самых недавно активных;
    /// 4) удалять самые старые гайды (до 50 за один вызов).
    ///
    /// 5 — даже после всех этапов уложиться не удалось (превысит лимит на
    /// следующем сообщении и повторит очистку).
    pub fn enforce_limit(&mut self, limit_bytes: u64) -> rusqlite::Result<u8> {
        // Цель — 95% лимита, чтобы чистка не запускалась на каждом сообщении.
        let soft_limit = limit_bytes - limit_bytes / 20;
        if self.db_disk_size() <= soft_limit {
            return Ok(0);
        }

        self.trim_all_chats(3000)?;
        self.vacuum();
        if self.db_disk_size() <= soft_limit {
            return Ok(1);
        }

        self.trim_all_chats(300)?;
        self.vacuum();
        if self.db_disk_size() <= soft_limit {
            return Ok(2);
        }

        self.conn.execute(
            "DELETE FROM users WHERE user_id NOT IN (
                SELECT user_id FROM users ORDER BY last_activity DESC LIMIT 500
            )",
            [],
        )?;
        self.vacuum();
        if self.db_disk_size() <= soft_limit {
            return Ok(3);
        }

        // Этап 4: удаляем до 50 самых старых гайдов одним запросом, затем
        // один VACUUM. VACUUM переписывает весь файл базы — на большой базе
        // это дорого, поэтому делаем его один раз, а не после каждого гайда.
        let deleted = self.conn.execute(
            "DELETE FROM guides WHERE guide_id IN (
                SELECT guide_id FROM guides ORDER BY created_at, guide_id LIMIT 50
            )",
            [],
        )?;
        if deleted == 0 {
            return Ok(5);
        }
        tracing::info!(deleted, "удалены старые гайды для освобождения места");
        self.vacuum();
        if self.db_disk_size() <= soft_limit {
            Ok(4)
        } else {
            Ok(5)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> MemoryStore {
        let path = std::env::temp_dir().join(format!("kostubetai_test_{}_{}.db", std::process::id(), name));
        let _ = std::fs::remove_file(&path);
        for suffix in ["-wal", "-shm"] {
            let mut os = path.clone().into_os_string();
            os.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(os));
        }
        MemoryStore::open(path.to_str().unwrap()).expect("открыть тестовую базу")
    }

    /// Симулирует открытие базы данных старого формата (без chat_id и display_name).
    #[test]
    fn migrate_old_schema_succeeds() {
        let path = std::env::temp_dir()
            .join(format!("kostubetai_test_{}_old_schema.db", std::process::id()));
        for p in [&path] {
            let _ = std::fs::remove_file(p);
        }
        for suffix in ["-wal", "-shm"] {
            let mut os = path.clone().into_os_string();
            os.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(os));
        }

        // Создаём базу со старой схемой (без chat_id / display_name)
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (user_id INTEGER PRIMARY KEY, first_seen INTEGER NOT NULL, last_activity INTEGER NOT NULL);
                 CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, created_at INTEGER NOT NULL);
                 CREATE INDEX idx_messages_user ON messages(user_id, id);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO users(user_id, first_seen, last_activity) VALUES(1, 0, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages(user_id, role, content, created_at) VALUES(1, 'user', 'старое сообщение', 0)",
                [],
            )
            .unwrap();
        }

        // Переоткрываем — должен сработать SCHEMA + migrate()
        let store = MemoryStore::open(path.to_str().unwrap())
            .expect("миграция старой базы должна пройти без ошибок");

        // Старые сообщения доступны через chat_id=0 (DEFAULT после ALTER TABLE)
        let hist = store.chat_messages_within_tokens(0, 1000).unwrap();
        assert!(!hist.is_empty(), "старые сообщения должны быть доступны");

        // Новые сообщения с реальным chat_id работают
        store
            .add_message(2, 500, "Тест", "user", "новое сообщение")
            .unwrap();
        let hist2 = store.chat_messages_within_tokens(500, 1000).unwrap();
        assert_eq!(hist2.len(), 1);
        assert_eq!(hist2[0].content, "новое сообщение");
        assert_eq!(hist2[0].display_name.as_deref(), Some("Тест"));

        // chat_participant_count работает на новом чате
        assert_eq!(store.chat_participant_count(500).unwrap(), 1);

        drop(store);
        let _ = std::fs::remove_file(&path);
        for suffix in ["-wal", "-shm"] {
            let mut os = path.clone().into_os_string();
            os.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(os));
        }
    }

    #[test]
    fn messages_respect_token_budget() {
        let store = temp_db("budget");
        // 30 сообщений по ~30 токенов — 900 токенов; бюджет 200 → последние ~7.
        for i in 0..30 {
            store.add_message(1, 100, "Алиса", "user", &format!("сообщение номер {i} {}", "х".repeat(80))).unwrap();
        }
        let kept = store.chat_messages_within_tokens(100, 200).unwrap();
        assert!(!kept.is_empty());
        assert!(kept.len() < 30);
        assert!(kept.last().unwrap().content.contains("номер 29"));

        let deleted = store.trim_chat_history(100, 200).unwrap();
        assert!(deleted > 0);
        let (count, tokens, _) = store.chat_stats(100).unwrap();
        assert_eq!(count as usize, kept.len());
        assert!(tokens <= 250, "занято {tokens} токенов при бюджете 200");

        // После обрезки повторный выбор возвращает то же окно.
        let again = store.chat_messages_within_tokens(100, 200).unwrap();
        assert_eq!(again.len(), kept.len());
        assert_eq!(again.last().unwrap().content, kept.last().unwrap().content);
    }

    #[test]
    fn semi_shared_chat_memory() {
        let store = temp_db("shared");
        // Два пользователя в одном чате — их сообщения перемешаны.
        store.add_message(1, 200, "Алиса", "user", "Привет!").unwrap();
        store.add_message(2, 200, "Борис", "user", "Здарова!").unwrap();
        store.add_message(1, 200, "Алиса", "assistant", "Привет, Борис!").unwrap();
        store.add_message(2, 200, "Борис", "user", "Как дела?").unwrap();

        // Все 4 сообщения видны в общем контексте чата.
        let hist = store.chat_messages_within_tokens(200, 1000).unwrap();
        assert_eq!(hist.len(), 4);
        assert_eq!(hist[0].content, "Привет!");
        assert_eq!(hist[0].display_name.as_deref(), Some("Алиса"));
        assert_eq!(hist[1].display_name.as_deref(), Some("Борис"));

        // Два уникальных участника.
        assert_eq!(store.chat_participant_count(200).unwrap(), 2);

        // Отдельный чат изолирован.
        store.add_message(3, 300, "Виктор", "user", "Привет из другого чата").unwrap();
        let other = store.chat_messages_within_tokens(300, 1000).unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].content, "Привет из другого чата");
    }

    #[test]
    fn reset_chat_clears_history() {
        let store = temp_db("reset");
        store.add_message(7, 500, "Админ", "user", "привет").unwrap();
        store.add_message(7, 500, "", "assistant", "привет!").unwrap();
        assert_eq!(store.chat_stats(500).unwrap().0, 2);
        store.reset_chat(500).unwrap();
        assert_eq!(store.chat_stats(500).unwrap().0, 0);
    }

    #[test]
    fn settings_crud() {
        let store = temp_db("settings");
        assert_eq!(store.get_setting("model").unwrap(), None);
        store.set_setting("model", "gpt-4o").unwrap();
        assert_eq!(store.get_setting("model").unwrap(), Some("gpt-4o".to_string()));
        store.set_setting("model", "gpt-4o-mini").unwrap();
        assert_eq!(store.get_setting("model").unwrap(), Some("gpt-4o-mini".to_string()));
        assert_eq!(store.all_settings().unwrap(), vec!["model".to_string()]);
        assert!(store.delete_setting("model").unwrap());
        assert_eq!(store.get_setting("model").unwrap(), None);
        assert!(!store.delete_setting("model").unwrap());
    }

    #[test]
    fn guides_lifecycle_and_search() {
        let mut store = temp_db("guides");
        let rust_guide = "Гайд по Rust\n\nCargo — система сборки. Крейты подключаются в Cargo.toml.\n\n\
                          Ownership и borrow checker — основа безопасности памяти в Rust.";
        let cook_guide = "Кулинарный гайд\n\nБорщ варится из свёклы и капусты. Подавать со сметаной.";
        assert_eq!(store.add_guide("rust", 1, rust_guide).unwrap(), 1);
        assert_eq!(store.add_guide("борщ", 1, cook_guide).unwrap(), 1);

        assert!(matches!(store.add_guide("rust", 1, "дубль").unwrap_err(), AddGuideError::Duplicate));
        assert!(matches!(store.add_guide("пустой", 1, "  ").unwrap_err(), AddGuideError::Empty));

        let list = store.guides().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|g| g.title == "rust" && g.chars > 100));

        // Поиск по «борщ» находит кулинарный гайд, а не гайд по Rust.
        let words = meaningful_words("как приготовить борщ?");
        let hits = store.search_guide_chunks(&words, 4000).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "борщ");
        assert!(hits[0].1.contains("свёклы"));

        // Поиск по «cargo crate» находит гайд по Rust.
        let words = meaningful_words("как подключить crate в cargo");
        let hits = store.search_guide_chunks(&words, 4000).unwrap();
        assert!(hits.iter().any(|(title, _)| title == "rust"));

        // find_guide: по id, точному названию и префиксу.
        assert_eq!(store.find_guide("1").unwrap(), Some(1));
        assert_eq!(store.find_guide("rust").unwrap(), Some(1));
        assert_eq!(store.find_guide("бор").unwrap(), Some(2));
        assert_eq!(store.find_guide("нет такого").unwrap(), None);

        assert!(store.delete_guide(1).unwrap());
        assert!(!store.delete_guide(1).unwrap());
        assert_eq!(store.guides_count().unwrap(), 1);
    }

    #[test]
    fn enforce_limit_trims_histories() {
        let mut store = temp_db("limit");
        // Малая база — лимит срабатывать не должен.
        for i in 0..10 {
            store.add_message(99, 900, "Тест", "user", &format!("лёгкое сообщение {i}")).unwrap();
        }
        assert_eq!(store.enforce_limit(1024 * 1024).unwrap(), 0);

        let payload = "д".repeat(300);
        for user in 1..=3 {
            for i in 0..300 {
                store.add_message(user, 900, &format!("Юзер{user}"), "user", &format!("{i} {payload}")).unwrap();
            }
        }
        let before = store.db_disk_size();
        assert!(before > 64 * 1024, "база должна была вырасти: {before} байт");

        let stage = store.enforce_limit(60 * 1024).unwrap();
        let after = store.db_disk_size();
        assert!(after < before / 2, "чистка должна заметно уменьшить базу: {after} из {before}");
        assert!((1..=3).contains(&stage));
        let (count, _, _) = store.chat_stats(900).unwrap();
        // 3000 символов бюджета / ~310 символов на сообщение — не больше пары десятков.
        assert!(count < 30, "в чате осталось {count} сообщений");
    }
}
