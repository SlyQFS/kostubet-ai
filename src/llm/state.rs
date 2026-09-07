use std::sync::RwLock;

/// Активная модель: атомарное (в рамках процесса) переключение по индексу.
#[derive(Debug)]
pub struct ModelState {
    models: Vec<String>,
    current: RwLock<usize>,
}

#[derive(Debug, thiserror::Error)]
#[error("индекс модели {0} вне списка из {1} моделей")]
pub struct BadModelIndex(pub usize, pub usize);

impl ModelState {
    pub fn new(models: Vec<String>) -> Self {
        debug_assert!(!models.is_empty(), "список моделей не должен быть пустым");
        Self {
            models,
            current: RwLock::new(0),
        }
    }

    pub fn list(&self) -> &[String] {
        &self.models
    }

    pub fn active_index(&self) -> usize {
        self.current.read().map(|guard| *guard).unwrap_or(0)
    }

    pub fn current(&self) -> String {
        self.models
            .get(self.active_index())
            .cloned()
            .unwrap_or_default()
    }

    /// Переключить модель по индексу, возвращает новое имя.
    pub fn set_index(&self, index: usize) -> Result<String, BadModelIndex> {
        if index >= self.models.len() {
            return Err(BadModelIndex(index, self.models.len()));
        }
        if let Ok(mut guard) = self.current.write() {
            *guard = index;
        }
        Ok(self.models[index].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_and_read() {
        let st = ModelState::new(vec!["a".into(), "b".into()]);
        assert_eq!(st.current(), "a");
        assert_eq!(st.set_index(1).unwrap(), "b");
        assert_eq!(st.current(), "b");
        assert!(st.set_index(2).is_err());
        assert_eq!(st.current(), "b");
    }
}
