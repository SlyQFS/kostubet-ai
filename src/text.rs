use std::collections::HashSet;

/// Человекочитаемый размер в байтах.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Делит текст на части не длиннее `limit` байт, стараясь не рвать слова и абзацы.
/// Нужно из-за лимита Telegram в 4096 символов на сообщение.
pub fn split_telegram(text: &str, limit: usize) -> Vec<String> {
    let text = text.trim();
    if text.len() <= limit {
        return vec![text.to_string()];
    }

    let mut pieces: Vec<String> = Vec::new();
    for paragraph in text.split("\n\n") {
        if paragraph.len() <= limit {
            pieces.push(paragraph.to_string());
            continue;
        }
        let mut rest = paragraph;
        while rest.len() > limit {
            let mut cut = limit;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            if let Some(ws) = rest[..cut].rfind(char::is_whitespace) {
                if ws >= limit / 2 {
                    cut = ws;
                }
            }
            pieces.push(rest[..cut].to_string());
            rest = &rest[cut..];
        }
        pieces.push(rest.to_string());
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for piece in pieces {
        let separator_len = if current.is_empty() { 0 } else { 2 };
        if !current.is_empty() && current.len() + separator_len + piece.len() > limit {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(&piece);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Делит гайд на фрагменты примерно по `target` символов (не больше `hard_limit`),
/// по границам абзацев — для поиска релевантных выдержек.
pub fn split_chunks(text: &str, target: usize, hard_limit: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let mut piece = paragraph.trim().to_string();
        while piece.chars().count() > hard_limit {
            // Чрезмерно длинный абзац режем жёстко, по границе символа.
            let head: String = piece.chars().take(target).collect();
            let tail = piece.chars().skip(target).collect::<String>();
            chunks.push(head);
            piece = tail;
        }
        if current.is_empty() {
            current = piece;
        } else if current.chars().count() + piece.chars().count() <= target {
            current.push_str("\n\n");
            current.push_str(&piece);
        } else {
            chunks.push(std::mem::take(&mut current));
            current = piece;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Набор значимых слов (нижний регистр, длина >= 3) для оценки релевантности.
pub fn meaningful_words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_units() {
        assert_eq!(fmt_bytes(0), "0.0 Б");
        assert_eq!(fmt_bytes(1024), "1.0 КБ");
        assert_eq!(fmt_bytes(1024 * 1024 * 1024), "1.0 ГБ");
    }

    #[test]
    fn split_telegram_short_text_is_single_chunk() {
        let chunks = split_telegram("привет", 100);
        assert_eq!(chunks, vec!["привет".to_string()]);
    }

    #[test]
    fn split_telegram_respects_limit() {
        let long = "слово ".repeat(3000);
        let chunks = split_telegram(&long, 3900);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 3900);
        }
        let total: usize = chunks.iter().map(String::len).sum();
        assert!(total + chunks.len() >= long.trim().len());
    }

    #[test]
    fn split_chunks_by_paragraphs() {
        let text = "абзац один\n\nабзац два\n\nабзац три";
        let chunks = split_chunks(text, 22, 100);
        assert!(!chunks.is_empty());
        let joined = chunks.join("\n\n");
        assert!(joined.contains("абзац один"));
        assert!(joined.contains("абзац три"));
    }

    #[test]
    fn split_chunks_hard_splits_long_paragraph() {
        let text = "x".repeat(5000);
        let chunks = split_chunks(&text, 1000, 2000);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.chars().count() <= 2000);
        }
    }

    #[test]
    fn meaningful_words_filters_short_and_case() {
        let words = meaningful_words("Привет, Мир! и а bb Rust");
        assert!(words.contains("привет"));
        assert!(words.contains("rust"));
        assert!(!words.contains("и"));
        assert!(!words.contains("bb"));
    }
}
