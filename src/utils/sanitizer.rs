use crate::storage::UserState;

/// Извлекает служебные теги `<user:...>` из ответа модели.
/// Возвращает вычищенный текст и список обновлённых состояний.
pub fn extract_user_tags(text: &str) -> (String, Vec<UserState>) {
    let mut states = Vec::new();
    let mut clean = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("<user:") {
        clean.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end_rel) = after.find('>') else {
            // Незакрытый тег — обрезаем хвост целиком, модель начала служебный блок.
            rest = "";
            break;
        };
        let inner = &after["<user:".len()..end_rel];
        if let Some(state) = parse_tag_inner(inner) {
            states.push(state);
        }
        rest = &after[end_rel + 1..];
    }
    clean.push_str(rest);

    let clean = clean
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    (clean, states)
}

/// Парсинг содержимого тега: `@nick goal="..." current="..." id=123`.
/// Обязателен только `id`; пустой `id` — тег игнорируется.
fn parse_tag_inner(inner: &str) -> Option<UserState> {
    let mut state = UserState::default();
    let mut display_name = String::new();

    let mut tokens = inner.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        if let Some(nick) = token.strip_prefix('@') {
            display_name = nick.trim_end_matches(',').to_string();
        } else if let Some((key, raw_value)) = split_kv(token) {
            let mut value = raw_value.to_string();
            // Значение могло содержать пробелы и съесть следующие токены до кавычки.
            if value.starts_with('"') && !value.ends_with('"') {
                for next in tokens.by_ref() {
                    value.push(' ');
                    value.push_str(next);
                    if next.ends_with('"') {
                        break;
                    }
                }
            }
            let value = value.trim_matches('"');
            match key {
                "id" => state.user_id = value.parse().unwrap_or(0),
                "goal" => state.goal = value.to_string(),
                "current" => state.current = value.to_string(),
                _ => {}
            }
        }
    }

    if state.user_id == 0 {
        return None;
    }
    if !display_name.is_empty() {
        state.display_name = display_name;
    }
    Some(state)
}

fn split_kv(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once('=')?;
    let key = key.trim_matches('"');
    if key.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Удаление упоминаний бота из текста ответа.
pub fn remove_bot_mention(text: &str, bot_username: &str) -> String {
    if bot_username.is_empty() {
        return text.to_string();
    }
    let target = format!("@{bot_username}");
    text.replace(&target, "")
        .replace(&format!("@{}", bot_username.to_lowercase()), "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_strips_tags() {
        let answer = "Сначала установи драйвер.\n<user:@vasya id=123 goal=\"прошить телефон\" current=\"установил adb\">\n<user:@petya id=5 current=\"ждёт кабель\">";
        let (clean, states) = extract_user_tags(answer);
        assert_eq!(clean, "Сначала установи драйвер.");
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].user_id, 123);
        assert_eq!(states[0].display_name, "vasya");
        assert_eq!(states[0].goal, "прошить телефон");
        assert_eq!(states[0].current, "установил adb");
        assert_eq!(states[1].user_id, 5);
        assert_eq!(states[1].display_name, "petya");
        assert_eq!(states[1].current, "ждёт кабель");
    }

    #[test]
    fn answer_without_tags() {
        let (clean, states) = extract_user_tags("обычный ответ");
        assert_eq!(clean, "обычный ответ");
        assert!(states.is_empty());
    }

    #[test]
    fn unclosed_tag_truncated() {
        let (clean, states) = extract_user_tags("ответ <user:@a id=1 goal=\"незакрыт");
        assert_eq!(clean, "ответ");
        assert!(states.is_empty());
    }

    #[test]
    fn tag_without_id_ignored() {
        let (clean, states) = extract_user_tags("ок <user:@x goal=\"y\"> конец");
        assert_eq!(clean, "ок  конец".trim());
        assert!(states.is_empty());
    }

    #[test]
    fn mention_removed() {
        assert_eq!(remove_bot_mention("@kostubet_bot как дела?", "kostubet_bot"), "как дела?");
        assert_eq!(remove_bot_mention("норм всё", "kostubet_bot"), "норм всё");
    }
}
