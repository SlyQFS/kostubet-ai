use crate::llm::client::ChatMessage;
use crate::storage::{StoredMessage, UserState};

pub const META_INSTRUCTION: &str = "\
Учитывай текущие мета-состояния участников выше. При ответе актуализируй \
служебный тег <user:...> в самом конце ответа.";

/// Всё необходимое для сборки запроса.
pub struct PromptParts<'a> {
    pub system_prompt: &'a str,
    /// Готовый контекст базы знаний (может быть пустым).
    pub knowledge: &'a str,
    pub user_states: &'a [UserState],
    pub history: &'a [StoredMessage],
    pub user_message: &'a str,
}

/// Сборка `[System + Knowledge + User Context] + [History] + [User]`.
/// Системная часть и мета-блоки не участвуют в обрезке истории
/// (обрезка происходит раньше — в хранилище).
pub fn build_messages(parts: PromptParts<'_>) -> Vec<ChatMessage> {
    let mut system = String::with_capacity(2048);
    system.push_str(parts.system_prompt.trim());

    if !parts.knowledge.trim().is_empty() {
        system.push_str("\n\n— База знаний —\n");
        system.push_str(parts.knowledge.trim());
        system.push_str("\n— Конец базы знаний —");
    }

    if !parts.user_states.is_empty() {
        system.push_str("\n\n— Контекст участников (user context) —\n");
        for st in parts.user_states {
            system.push_str(&st.to_tag());
            system.push('\n');
        }
        system.push_str(META_INSTRUCTION);
    }

    let mut messages = vec![ChatMessage::system(system)];

    for m in parts.history {
        match m.role {
            crate::storage::Role::Assistant => messages.push(ChatMessage::assistant(&m.content)),
            crate::storage::Role::User => {
                let named = if m.user_id != 0 && !m.display_name.is_empty() {
                    format!("{} (id={}): {}", m.display_name, m.user_id, m.content)
                } else if !m.display_name.is_empty() {
                    format!("{}: {}", m.display_name, m.content)
                } else {
                    m.content.clone()
                };
                messages.push(ChatMessage::user(named));
            }
        }
    }

    messages.push(ChatMessage::user(parts.user_message));
    messages
}

/// Форматирование карточек базы знаний в текст для промпта.
pub fn format_knowledge(hits: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (title, content) in hits {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("▪ {title}\n{content}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_composition() {
        let states = vec![UserState {
            user_id: 1,
            display_name: "vasya".into(),
            goal: "прошить".into(),
            current: "adb ok".into(),
        }];
        let history = vec![
            StoredMessage { user_id: 1, display_name: "vasya".into(), role: crate::storage::Role::User, content: "привет".into() },
            StoredMessage { user_id: 0, display_name: String::new(), role: crate::storage::Role::Assistant, content: "о".into() },
        ];
        let msgs = build_messages(PromptParts {
            system_prompt: "Ты бот.",
            knowledge: "▪ Загрузчик\nfastboot",
            user_states: &states,
            history: &history,
            user_message: "что дальше?",
        });

        assert_eq!(msgs.len(), 4);
        let sys = &msgs[0];
        assert_eq!(sys.role, "system");
        assert!(sys.content.starts_with("Ты бот."));
        assert!(sys.content.contains("▪ Загрузчик"));
        assert!(sys.content.contains("<user:@vasya goal=\"прошить\" current=\"adb ok\" id=1>"));
        assert!(sys.content.contains("Учитывай текущие мета-состояния"));
        assert_eq!(msgs[1].content, "vasya (id=1): привет");
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[3].content, "что дальше?");
    }

    #[test]
    fn minimal_composition() {
        let msgs = build_messages(PromptParts {
            system_prompt: "s",
            knowledge: "  ",
            user_states: &[],
            history: &[],
            user_message: "q",
        });
        assert_eq!(msgs.len(), 2);
        assert!(!msgs[0].content.contains("База знаний"));
        assert!(!msgs[0].content.contains("user context"));
    }
}
