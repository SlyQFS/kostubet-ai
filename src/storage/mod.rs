#[cfg(test)]
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::SqlitePool;

/// Роль сообщения в истории.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Сообщение истории диалога.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub user_id: i64,
    pub display_name: String,
    pub role: Role,
    pub content: String,
}

/// Мета-блок собеседника (`<user:@nick id=123 goal="..." current="...">`).
#[derive(Debug, Clone, Default)]
pub struct UserState {
    pub user_id: i64,
    pub display_name: String,
    pub goal: String,
    pub current: String,
}

impl UserState {
    /// Сериализация в тег для промпта. Пустые поля пропускаются.
    pub fn to_tag(&self) -> String {
        let mut parts = vec![format!("@{}", self.display_name)];
        if !self.goal.is_empty() {
            parts.push(format!("goal=\"{}\"", self.goal.replace('"', "'")));
        }
        if !self.current.is_empty() {
            parts.push(format!("current=\"{}\"", self.current.replace('"', "'")));
        }
        format!("<user:{} id={}>", parts.join(" "), self.user_id)
    }
}

pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Async-хранилище на sqlx/sqlite.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

pub type SharedStore = Arc<Store>;

/// Миграции грузятся рантаймом относительно рабочей директории
/// (в Docker каталог migrations/ копируется рядом с бинарником).
async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let migrator = sqlx::migrate::Migrator::new(std::path::Path::new("./migrations")).await?;
    migrator.run(pool).await?;
    Ok(())
}

impl Store {
    pub async fn connect(path: &str) -> Result<Self, sqlx::Error> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(sqlx::Error::Io)?;
            }
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        run_migrations(&pool).await?;
        Ok(Self { pool })
    }

    /// Пул для in-memory тестов.
    #[cfg(test)]
    pub async fn connect_in_memory() -> Result<Self, sqlx::Error> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        run_migrations(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn add_message(
        &self,
        chat_id: i64,
        thread_id: i64,
        user_id: i64,
        display_name: &str,
        role: Role,
        content: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO messages (chat_id, thread_id, user_id, display_name, role, content, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(chat_id)
        .bind(thread_id)
        .bind(user_id)
        .bind(display_name)
        .bind(role.as_str())
        .bind(content)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Последние сообщения диалога в пределах символьного бюджета
    /// (эвристика ~3 символа на токен). Идём от свежих к старым.
    pub async fn recent_messages(
        &self,
        chat_id: i64,
        thread_id: i64,
        budget_chars: usize,
    ) -> Result<Vec<StoredMessage>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (i64, String, String, String)>(
            "SELECT user_id, display_name, role, content FROM messages
             WHERE chat_id = ? AND thread_id = ?
             ORDER BY id DESC",
        )
        .bind(chat_id)
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::new();
        let mut used = 0usize;
        for (user_id, display_name, role, content) in rows {
            let len = content.len();
            if used + len > budget_chars && !out.is_empty() {
                break;
            }
            used += len;
            let role = if role == "assistant" { Role::Assistant } else { Role::User };
            out.push(StoredMessage { user_id, display_name, role, content });
        }
        out.reverse();
        Ok(out)
    }

    /// Скользящее окно: удаляет старейшие сообщения диалогов сверх бюджета.
    pub async fn trim_window(&self, budget_chars: usize) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            "DELETE FROM messages WHERE id IN (
                 SELECT id FROM (
                     SELECT id,
                            SUM(length(content)) OVER (
                                PARTITION BY chat_id, thread_id ORDER BY id DESC
                            ) AS total
                     FROM messages
                 )
                 WHERE total > ?
             )",
        )
        .bind(budget_chars as i64)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    pub async fn get_state(&self, user_id: i64) -> Result<Option<UserState>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String, String, String)>(
            "SELECT display_name, goal, current FROM user_states WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(display_name, goal, current)| UserState {
            user_id,
            display_name,
            goal,
            current,
        }))
    }

    pub async fn upsert_state(&self, state: &UserState) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO user_states (user_id, display_name, goal, current, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET
                display_name = CASE WHEN excluded.display_name = ''
                                    THEN user_states.display_name
                                    ELSE excluded.display_name END,
                goal = excluded.goal,
                current = excluded.current,
                updated_at = excluded.updated_at",
        )
        .bind(state.user_id)
        .bind(&state.display_name)
        .bind(&state.goal)
        .bind(&state.current)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Мета-блоки перечисленных участников диалога.
    pub async fn states_for_ids(&self, user_ids: &[i64]) -> Result<Vec<UserState>, sqlx::Error> {
        let mut out = Vec::with_capacity(user_ids.len());
        for id in user_ids {
            if let Some(state) = self.get_state(*id).await? {
                out.push(state);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Store {
        Store::connect_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn messages_roundtrip_and_window() {
        let s = store().await;
        for i in 0..5 {
            let long = format!("msg {i} {}", "x".repeat(100));
            s.add_message(1, 0, 10, "Vasya", Role::User, &long)
                .await
                .unwrap();
        }
        s.add_message(1, 0, 99, "", Role::Assistant, "answer")
            .await
            .unwrap();

        let recent = s.recent_messages(1, 0, 400).await.unwrap();
        assert_eq!(recent.len(), 4); // бюджет не вмещает всё
        assert_eq!(recent.last().unwrap().content, "answer");

        let trimmed = s.trim_window(400).await.unwrap();
        assert_eq!(trimmed, 2);
        let recent = s.recent_messages(1, 0, 10_000).await.unwrap();
        assert_eq!(recent.len(), 4);
        assert!(recent[0].content.starts_with("msg 2 "));
    }

    #[test]
    fn user_state_tag_format() {
        let st = UserState {
            user_id: 123,
            display_name: "nick".into(),
            goal: "прошить телефон".into(),
            current: "установил adb".into(),
        };
        assert_eq!(
            st.to_tag(),
            "<user:@nick goal=\"прошить телефон\" current=\"установил adb\" id=123>"
        );
        let empty = UserState {
            user_id: 5,
            ..Default::default()
        };
        assert_eq!(empty.to_tag(), "<user:@ id=5>");
    }

    #[tokio::test]
    async fn upsert_state_keeps_name_on_empty() {
        let s = store().await;
        s.upsert_state(&UserState {
            user_id: 1,
            display_name: "Nick".into(),
            goal: "g".into(),
            current: "c".into(),
        })
        .await
        .unwrap();
        s.upsert_state(&UserState {
            user_id: 1,
            display_name: String::new(),
            goal: "g2".into(),
            current: "c2".into(),
        })
        .await
        .unwrap();
        let st = s.get_state(1).await.unwrap().unwrap();
        assert_eq!(st.display_name, "Nick");
        assert_eq!(st.goal, "g2");
    }

    #[tokio::test]
    async fn threads_are_isolated() {
        let s = store().await;
        s.add_message(1, 0, 1, "A", Role::User, "main").await.unwrap();
        s.add_message(1, 5, 1, "A", Role::User, "topic").await.unwrap();
        let main = s.recent_messages(1, 0, 10_000).await.unwrap();
        let topic = s.recent_messages(1, 5, 10_000).await.unwrap();
        assert_eq!(main.len(), 1);
        assert_eq!(main[0].content, "main");
        assert_eq!(topic[0].content, "topic");
    }
}
