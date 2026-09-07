/// Деление по лимиту Telegram (символы, char_boundary-safe):
/// грейди по словам, сверхдлинные слова режутся по символам.
pub fn split_telegram(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(64);
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for word in text.split_inclusive(char::is_whitespace) {
        let word_len = word.chars().count();

        if word_len > limit {
            // слово само длиннее лимита — сбрасываем накопленное и режем по символам
            if !current.trim().is_empty() {
                chunks.push(current.trim_end().to_string());
            }
            current.clear();
            let mut buf = String::new();
            for ch in word.trim_end().chars() {
                buf.push(ch);
                if buf.chars().count() == limit {
                    chunks.push(std::mem::take(&mut buf));
                }
            }
            current = buf;
            current_len = current.chars().count();
            continue;
        }

        if current_len + word_len > limit && !current.is_empty() {
            chunks.push(current.trim_end().to_string());
            current.clear();
            current_len = 0;
        }
        current.push_str(word);
        current_len += word_len;
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim_end().to_string());
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Эвристика: ~3 символа на токен (для русского текста).
pub fn tokens_to_chars(tokens: usize) -> usize {
    tokens * 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_single_chunk() {
        assert_eq!(split_telegram("привет", 100), vec!["привет"]);
    }

    #[test]
    fn splits_by_limit() {
        let text = "слово ".repeat(30) + "конец";
        let chunks = split_telegram(&text, 64);
        assert!(chunks.iter().all(|c| c.chars().count() <= 64));
        assert_eq!(chunks.join(" "), text);
        assert!(chunks.len() > 1);
    }

    #[test]
    fn unicode_safe() {
        let text = "ж".repeat(300);
        let chunks = split_telegram(&text, 100);
        assert!(chunks.iter().all(|c| c.chars().count() <= 100));
        assert_eq!(chunks.concat().chars().count(), 300);
    }

    #[test]
    fn huge_word_hard_split() {
        let text = "ы".repeat(250);
        let chunks = split_telegram(&text, 100);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 100));
    }

    #[test]
    fn token_heuristic() {
        assert_eq!(tokens_to_chars(4000), 12_000);
    }
}
