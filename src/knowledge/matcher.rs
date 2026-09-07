use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use rust_stemmers::{Algorithm, Stemmer};
use tracing::{info, warn};

use super::loader::KnowledgeBase;

/// Минимальная длина слова для проверки.
const MIN_WORD_LEN: usize = 2;

/// Локальный матчер базы знаний: русский стемминг + точные ключи + семантика
/// через fastembed (ONNX, multilingual-e5-small).
pub struct Matcher {
    base: KnowledgeBase,
    stemmer: Stemmer,
    /// Точные строковые ключи в нижнем регистре (для сленга: "бл", "edl", "adb").
    card_keys_exact: Vec<HashSet<String>>,
    /// Стеммированные ключи карточек (для поиска без окончаний).
    card_keys_stemmed: Vec<HashSet<String>>,
    /// Стеммированное содержимое карточек.
    card_content_stemmed: Vec<HashSet<String>>,
    /// Векторные эмбеддинги карточек; пуст — если эмбеддер недоступен.
    card_embeddings: Vec<Vec<f32>>,
    embedder: Option<Arc<std::sync::Mutex<fastembed::TextEmbedding>>>,
    embed_lock: tokio::sync::Mutex<()>,
}

impl Matcher {
    /// Полный матчер с семантикой. Тяжёлый (ONNX-сессия) — запускать
    /// через `tokio::task::spawn_blocking` на старте.
    pub fn new(base: KnowledgeBase, cache_dir: &str) -> Self {
        let mut matcher = Self::without_semantic(base);

        match fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed::EmbeddingModel::MultilingualE5Small)
                .with_cache_dir(PathBuf::from(cache_dir))
                .with_show_download_progress(true),
        ) {
            Ok(mut model) => {
                info!("семантический эмбеддер базы знаний готов (multilingual-e5-small)");
                matcher.compute_card_embeddings(&mut model);
                matcher.embedder = Some(Arc::new(std::sync::Mutex::new(model)));
            }
            Err(e) => {
                warn!("эмбеддер недоступен ({e}); база знаний работает на стемминге и ключевых словах");
            }
        }
        matcher
    }

    /// Лёгкий матчер без семантики (тесты, деградация).
    pub fn without_semantic(base: KnowledgeBase) -> Self {
        let stemmer = Stemmer::create(Algorithm::Russian);

        let card_keys_exact: Vec<HashSet<String>> = base
            .cards
            .iter()
            .map(|c| {
                let mut set = HashSet::new();
                for k in &c.keys {
                    let k_lower = k.trim().to_lowercase();
                    if !k_lower.is_empty() {
                        set.insert(k_lower.clone());
                    }
                    for w in k_lower.split(|ch: char| !ch.is_alphanumeric()) {
                        let w = w.trim();
                        if w.chars().count() >= MIN_WORD_LEN {
                            set.insert(w.to_string());
                        }
                    }
                }
                set
            })
            .collect();

        let card_keys_stemmed: Vec<HashSet<String>> = base
            .cards
            .iter()
            .map(|c| {
                c.keys
                    .iter()
                    .flat_map(|k| {
                        k.split(|ch: char| !ch.is_alphanumeric())
                            .filter(|w| w.chars().count() >= 3)
                    })
                    .map(|w| stemmer.stem(&w.to_lowercase()).to_string())
                    .collect()
            })
            .collect();

        let card_content_stemmed: Vec<HashSet<String>> = base
            .cards
            .iter()
            .map(|c| {
                c.content
                    .split(|ch: char| !ch.is_alphanumeric())
                    .filter(|w| w.chars().count() >= 3)
                    .map(|w| stemmer.stem(&w.to_lowercase()).to_string())
                    .collect()
            })
            .collect();

        Self {
            base,
            stemmer,
            card_keys_exact,
            card_keys_stemmed,
            card_content_stemmed,
            card_embeddings: Vec::new(),
            embedder: None,
            embed_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn keyword_scores(&self, query: &str) -> Vec<f32> {
        let q_lower = query.to_lowercase();
        let q_tokens: Vec<&str> = q_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= MIN_WORD_LEN)
            .collect();

        let q_stems: HashSet<String> = q_tokens
            .iter()
            .filter(|w| w.chars().count() >= 3)
            .map(|w| self.stemmer.stem(w).to_string())
            .collect();

        self.base
            .cards
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let mut score = 0.0f32;
                // Точное совпадение слова с ключами (например "бл", "adb", "edl")
                for token in &q_tokens {
                    if self.card_keys_exact[i].contains(*token) {
                        score += 3.0;
                    }
                }
                // Совпадение по стеммам (без окончаний)
                let key_stem_hits = q_stems.intersection(&self.card_keys_stemmed[i]).count() as f32;
                let content_stem_hits = q_stems.intersection(&self.card_content_stemmed[i]).count() as f32;
                score + key_stem_hits * 2.0 + content_stem_hits * 0.5
            })
            .collect()
    }

    fn compute_card_embeddings(&mut self, model: &mut fastembed::TextEmbedding) {
        let texts: Vec<String> = self
            .base
            .cards
            .iter()
            .map(|c| format!("passage: {}. {}", c.title, c.content))
            .collect();
        match model.embed(texts, None) {
            Ok(embs) => self.card_embeddings = embs,
            Err(e) => warn!("не удалось встроить карточки базы знаний: {e}"),
        }
    }

    async fn embed_query(&self, query: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_ref()?.clone();
        let text = format!("query: {query}");
        let _guard = self.embed_lock.lock().await;
        let embs = tokio::task::spawn_blocking(move || match embedder.lock() {
            Ok(mut guard) => guard.embed(vec![text], None),
            Err(_) => Ok(Vec::new()),
        })
        .await
        .ok()?
        .ok()?;
        embs.into_iter().next()
    }

    /// Подбор карточек под запрос: семантика (cosine >= threshold) +
    /// keyword-попадания по ключам. Возвращает `(title, content)`.
    pub async fn match_query(
        &self,
        query: &str,
        top_n: usize,
        threshold: f32,
        max_chars: usize,
    ) -> Vec<(String, String)> {
        if self.base.cards.is_empty() {
            return Vec::new();
        }
        let keywords = self.keyword_scores(query);
        let semantic: Option<Vec<f32>> = self.embed_query(query).await;

        let mut candidates: Vec<(usize, f32, f32)> = self
            .base
            .cards
            .iter()
            .enumerate()
            .filter_map(|(i, _)| {
                let kw = keywords[i];
                let sem = semantic
                    .as_ref()
                    .and_then(|q| self.card_embeddings.get(i).map(|c| cosine(q, c)))
                    .unwrap_or(0.0);
                let passes = sem >= threshold || kw >= 2.0;
                passes.then_some((i, sem, kw))
            })
            .collect();

        candidates.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
        });

        let mut out = Vec::new();
        let mut used = 0usize;
        for (i, _, _) in candidates {
            let card = &self.base.cards[i];
            let cost = card.content.len() + card.title.len();
            if used + cost > max_chars && !out.is_empty() {
                break;
            }
            used += cost;
            out.push((card.title.clone(), card.content.clone()));
            if out.len() >= top_n {
                break;
            }
        }
        out
    }

    pub fn has_semantic(&self) -> bool {
        self.embedder.is_some()
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::loader::KnowledgeCard;

    fn test_base() -> KnowledgeBase {
        KnowledgeBase {
            cards: vec![
                KnowledgeCard {
                    title: "Разблокировка загрузчика".into(),
                    keys: vec!["bootloader".into(), "загрузчик".into(), "разблокировка".into(), "разлочить".into(), "бл".into()],
                    content: "Разблокировка загрузчика выполняется командой fastboot flashing unlock.".into(),
                },
                KnowledgeCard {
                    title: "Драйверы ADB и Fastboot".into(),
                    keys: vec!["драйвер".into(), "adb".into(), "дрова".into(), "fastboot".into()],
                    content: "Драйвер adb ставится через Google USB Driver, после чего телефон виден в adb devices.".into(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn keyword_match_with_endings_and_slang() {
        let m = Matcher::without_semantic(test_base());
        // Проверяем со сленгом "бл"
        let hits = m.match_query("как разлочить бл?", 4, 0.99, 4000).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "Разблокировка загрузчика");

        // Проверяем с падежами ("загрузчиком", "драйверами")
        let hits2 = m.match_query("проблема с драйверами", 4, 0.99, 4000).await;
        assert_eq!(hits2.len(), 1);
        assert_eq!(hits2[0].0, "Драйверы ADB и Fastboot");
    }

    #[tokio::test]
    async fn no_match_on_unrelated() {
        let m = Matcher::without_semantic(test_base());
        let hits = m.match_query("как сварить борщ", 4, 0.99, 4000).await;
        assert!(hits.is_empty());
    }
}
