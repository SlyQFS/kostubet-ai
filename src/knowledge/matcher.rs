use std::collections::HashSet;

use rust_stemmers::{Algorithm, Stemmer};

use super::loader::KnowledgeBase;

/// Минимальная длина слова для проверки.
const MIN_WORD_LEN: usize = 2;

/// Минимальный порог релевантности для включения карточки в ответ.
const MIN_SCORE: f32 = 1.5;

/// Локальный матчер базы знаний на русском стемминге (Snowball)
/// и точном сопоставлении ключевых слов / сленга ("бл", "edl", "adb", "magisk").
pub struct Matcher {
    base: KnowledgeBase,
    stemmer: Stemmer,
    /// Точные строковые ключи в нижнем регистре (для сленга: "бл", "edl", "adb").
    card_keys_exact: Vec<HashSet<String>>,
    /// Стеммированные ключи карточек (для поиска без окончаний).
    card_keys_stemmed: Vec<HashSet<String>>,
    /// Стеммированное содержимое карточек.
    card_content_stemmed: Vec<HashSet<String>>,
}

impl Matcher {
    pub fn new(base: KnowledgeBase) -> Self {
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
                // Совпадение по стеммам без окончаний
                let key_stem_hits = q_stems.intersection(&self.card_keys_stemmed[i]).count() as f32;
                let content_stem_hits = q_stems.intersection(&self.card_content_stemmed[i]).count() as f32;
                score + key_stem_hits * 2.0 + content_stem_hits * 0.5
            })
            .collect()
    }

    /// Подбор карточек под запрос по стеммингу и ключевым словам.
    /// Возвращает `Vec<(title, content)>`.
    pub async fn match_query(
        &self,
        query: &str,
        top_n: usize,
        _threshold: f32,
        max_chars: usize,
    ) -> Vec<(String, String)> {
        if self.base.cards.is_empty() {
            return Vec::new();
        }
        let scores = self.keyword_scores(query);

        let mut candidates: Vec<(usize, f32)> = scores
            .into_iter()
            .enumerate()
            .filter(|(_, score)| *score >= MIN_SCORE)
            .collect();

        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut out = Vec::new();
        let mut used = 0usize;
        for (i, _) in candidates {
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
                    keys: vec![
                        "bootloader".into(),
                        "загрузчик".into(),
                        "разблокировка".into(),
                        "разлочить".into(),
                        "бл".into(),
                    ],
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
        let m = Matcher::new(test_base());
        // Проверяем со сленгом "бл"
        let hits = m.match_query("как разлочить бл?", 4, 0.0, 4000).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "Разблокировка загрузчика");

        // Проверяем с падежами ("загрузчиком", "драйверами")
        let hits2 = m.match_query("проблема с драйверами", 4, 0.0, 4000).await;
        assert_eq!(hits2.len(), 1);
        assert_eq!(hits2[0].0, "Драйверы ADB и Fastboot");
    }

    #[tokio::test]
    async fn no_match_on_unrelated() {
        let m = Matcher::new(test_base());
        let hits = m.match_query("как сварить борщ", 4, 0.0, 4000).await;
        assert!(hits.is_empty());
    }
}

