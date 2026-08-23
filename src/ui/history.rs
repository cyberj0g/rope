use std::{io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

pub struct PromptHistory {
    path: PathBuf,
    entries: Vec<String>,
    index: Option<usize>,
    draft: String,
}

impl PromptHistory {
    pub async fn load() -> Result<Self> {
        let base = directories::BaseDirs::new().context("home directory not found")?;
        let path = base.data_dir().join("harness/history.jsonl");
        Self::load_path(path).await
    }

    async fn load_path(path: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(data) => data,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let mut entries = data
            .lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(index, line)| {
                serde_json::from_str(line)
                    .with_context(|| format!("parse prompt history line {}", index + 1))
            })
            .collect::<Result<Vec<_>>>()?;
        entries.dedup();
        Ok(Self {
            path,
            entries,
            index: None,
            draft: String::new(),
        })
    }

    pub async fn record(&mut self, prompt: &str) -> Result<()> {
        if self.entries.last().is_some_and(|entry| entry == prompt) {
            self.reset_navigation();
            return Ok(());
        }
        self.entries.push(prompt.to_owned());
        self.reset_navigation();
        let mut line = serde_json::to_vec(prompt)?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        file.write_all(&line).await?;
        file.flush().await?;
        Ok(())
    }

    pub fn previous(&mut self, input: &mut String) {
        if self.entries.is_empty() {
            return;
        }
        let index = match self.index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft.clone_from(input);
                self.entries.len() - 1
            }
        };
        self.index = Some(index);
        input.clone_from(&self.entries[index]);
    }

    pub fn next(&mut self, input: &mut String) {
        let Some(index) = self.index else {
            return;
        };
        if index + 1 < self.entries.len() {
            self.index = Some(index + 1);
            input.clone_from(&self.entries[index + 1]);
        } else {
            self.index = None;
            input.clone_from(&self.draft);
            self.draft.clear();
        }
    }

    pub fn reset_navigation(&mut self) {
        self.index = None;
        self.draft.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigates_history_and_restores_draft() {
        let mut history = PromptHistory {
            path: PathBuf::new(),
            entries: vec!["first".into(), "second".into()],
            index: None,
            draft: String::new(),
        };
        let mut input = "unfinished".to_owned();

        history.previous(&mut input);
        assert_eq!(input, "second");
        history.previous(&mut input);
        assert_eq!(input, "first");
        history.next(&mut input);
        assert_eq!(input, "second");
        history.next(&mut input);
        assert_eq!(input, "unfinished");
    }

    #[tokio::test]
    async fn reloads_multiline_history_from_disk() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rope-history-test-{}-{unique}.jsonl",
            std::process::id()
        ));
        let mut history = PromptHistory::load_path(path.clone()).await.unwrap();
        history.record("first\nsecond").await.unwrap();

        let mut reloaded = PromptHistory::load_path(path.clone()).await.unwrap();
        let mut input = String::new();
        reloaded.previous(&mut input);
        assert_eq!(input, "first\nsecond");

        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn skips_sequential_duplicates() {
        let path = std::env::temp_dir().join(format!(
            "rope-history-dedup-test-{}.jsonl",
            std::process::id()
        ));
        let mut history = PromptHistory::load_path(path.clone()).await.unwrap();
        history.record("same").await.unwrap();
        history.record("same").await.unwrap();
        history.record("different").await.unwrap();
        history.record("same").await.unwrap();

        assert_eq!(history.entries, ["same", "different", "same"]);
        tokio::fs::remove_file(path).await.unwrap();
    }
}
