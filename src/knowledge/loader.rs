use std::path::Path;

use serde::Deserialize;

/// Карточка базы знаний: `knowledge/*.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct KnowledgeCard {
    pub title: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(deserialize_with = "deserialize_content")]
    pub content: String,
}

fn deserialize_content<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawContent {
        Single(String),
        Lines(Vec<String>),
    }

    match RawContent::deserialize(deserializer)? {
        RawContent::Single(s) => Ok(s),
        RawContent::Lines(lines) => Ok(lines.join("\n")),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CardOrCards {
    Single(KnowledgeCard),
    Multiple(Vec<KnowledgeCard>),
}

#[derive(Debug, Clone, Default)]
pub struct KnowledgeBase {
    pub cards: Vec<KnowledgeCard>,
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("папка базы знаний «{0}» не найдена")]
    MissingDir(String),
    #[error("ошибка чтения {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("битый JSON в {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

impl KnowledgeBase {
    /// Загрузка карточек из папки. Поддерживаются как одиночные карточки, так и JSON-массивы.
    pub fn load(dir: &str) -> Result<Self, KnowledgeError> {
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            return Err(KnowledgeError::MissingDir(dir.to_string()));
        }

        let mut cards = Vec::new();
        for entry in std::fs::read_dir(dir_path)
            .map_err(|source| KnowledgeError::Io { path: dir.to_string(), source })?
        {
            let entry =
                entry.map_err(|source| KnowledgeError::Io { path: dir.to_string(), source })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            let text = std::fs::read_to_string(&path).map_err(|source| KnowledgeError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let parsed: CardOrCards = serde_json::from_str(&text)
                .map_err(|e| KnowledgeError::Parse { path: path.display().to_string(), source: e })?;

            match parsed {
                CardOrCards::Single(card) => {
                    if !card.title.trim().is_empty() && !card.content.trim().is_empty() {
                        cards.push(card);
                    }
                }
                CardOrCards::Multiple(list) => {
                    for card in list {
                        if !card.title.trim().is_empty() && !card.content.trim().is_empty() {
                            cards.push(card);
                        }
                    }
                }
            }
        }
        cards.sort_by_key(|c| c.title.to_lowercase());

        Ok(Self { cards })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_single_and_array_cards() {
        let tmp = std::env::temp_dir().join(format!("kb-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Одиночная карточка
        std::fs::write(
            tmp.join("bootloader.json"),
            r#"{"title":"Загрузчик","keys":["bootloader","fastboot"],"content":"Разблокировка через fastboot flashing unlock"}"#,
        )
        .unwrap();
        // Массив карточек
        std::fs::write(
            tmp.join("multi.json"),
            r#"[
                {"title":"Карточка 1","keys":["один"],"content":"Текст 1"},
                {"title":"Карточка 2","keys":["два"],"content":"Текст 2"}
            ]"#,
        )
        .unwrap();
        // Игнорируем не-JSON
        std::fs::write(tmp.join("notes.txt"), "ignore me").unwrap();

        // Карточка с массивом строк в content
        std::fs::write(
            tmp.join("lines.json"),
            r#"{"title":"Строки","keys":["тест"],"content":["Первая строка","Вторая строка"]}"#,
        )
        .unwrap();

        let kb = KnowledgeBase::load(tmp.to_str().unwrap()).unwrap();
        assert_eq!(kb.cards.len(), 4);
        let lines_card = kb.cards.iter().find(|c| c.title == "Строки").unwrap();
        assert_eq!(lines_card.content, "Первая строка\nВторая строка");

        std::fs::remove_dir_all(&tmp).unwrap();

        // Проверяем реальную папку knowledge репозитория
        if std::path::Path::new("knowledge").is_dir() {
            let real_kb = KnowledgeBase::load("knowledge").unwrap();
            assert_eq!(real_kb.cards.len(), 6);
            for card in &real_kb.cards {
                assert!(!card.title.is_empty());
                assert!(!card.keys.is_empty());
                assert!(!card.content.is_empty());
            }
        }
    }

    #[test]
    fn missing_dir_is_error() {
        assert!(matches!(
            KnowledgeBase::load("/nonexistent-kb-xyz"),
            Err(KnowledgeError::MissingDir(_))
        ));
    }
}
