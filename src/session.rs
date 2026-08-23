use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{config::Startup, runtime::Message};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionMeta {
    pub name: String,
    pub created_at: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default)]
    pub compaction_summary: Option<String>,
    #[serde(default)]
    pub compacted_through: usize,
}

pub struct Session {
    root: PathBuf,
    pub meta: SessionMeta,
}

impl Session {
    pub async fn open(startup: Startup) -> Result<(Self, Vec<Message>)> {
        let root = sessions_root()?;
        tokio::fs::create_dir_all(&root).await?;
        if let Some(name) = startup.continue_session {
            let name = if name == "latest" {
                latest(&root).await?
            } else {
                clean_name(&name)?
            };
            return Self::load(root, name).await;
        }
        let name = startup
            .session
            .map(|name| clean_name(&name))
            .transpose()?
            .unwrap_or_else(auto_name);
        let session = Self::create(root, name).await?;
        Ok((session, Vec::new()))
    }

    pub async fn new_named(name: Option<String>) -> Result<Self> {
        let root = sessions_root()?;
        tokio::fs::create_dir_all(&root).await?;
        let name = name
            .map(|name| clean_name(&name))
            .transpose()?
            .unwrap_or_else(auto_name);
        Self::create(root, name).await
    }

    async fn create(root: PathBuf, name: String) -> Result<Self> {
        let directory = root.join(&name);
        if directory.exists() {
            bail!("session already exists: {name}");
        }
        tokio::fs::create_dir(&directory).await?;
        let session = Self {
            root,
            meta: SessionMeta {
                name,
                created_at: now(),
                total_tokens: 0,
                context_tokens: 0,
                compaction_summary: None,
                compacted_through: 0,
            },
        };
        session.save().await?;
        tokio::fs::write(session.messages_path(), []).await?;
        Ok(session)
    }

    async fn load(root: PathBuf, name: String) -> Result<(Self, Vec<Message>)> {
        let directory = root.join(&name);
        let meta: SessionMeta = serde_json::from_slice(
            &tokio::fs::read(directory.join("session.json"))
                .await
                .with_context(|| format!("load session {name}"))?,
        )?;
        let data = tokio::fs::read_to_string(directory.join("messages.jsonl"))
            .await
            .unwrap_or_default();
        let messages = data
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((Self { root, meta }, messages))
    }

    pub async fn append(&self, messages: &[Message]) -> Result<()> {
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(self.messages_path())
            .await?;
        for message in messages {
            file.write_all(&serde_json::to_vec(message)?).await?;
            file.write_all(b"\n").await?;
        }
        file.flush().await?;
        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        tokio::fs::write(
            self.directory().join("session.json"),
            serde_json::to_vec_pretty(&self.meta)?,
        )
        .await?;
        Ok(())
    }

    fn directory(&self) -> PathBuf {
        self.root.join(&self.meta.name)
    }
    fn messages_path(&self) -> PathBuf {
        self.directory().join("messages.jsonl")
    }
}

fn sessions_root() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("home directory not found")?;
    Ok(base.data_dir().join("harness/sessions"))
}

fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        bail!("invalid session name");
    }
    Ok(name.to_owned())
}

fn auto_name() -> String {
    format!("session-{}", now())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn latest(root: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(root).await?;
    let mut latest = None;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.metadata().await?.is_dir() {
            continue;
        }
        let directory = entry.path();
        let mut modified = entry.metadata().await?.modified().unwrap_or(UNIX_EPOCH);
        for file in ["session.json", "messages.jsonl"] {
            if let Ok(metadata) = tokio::fs::metadata(directory.join(file)).await {
                modified = modified.max(metadata.modified().unwrap_or(UNIX_EPOCH));
            }
        }
        if latest.as_ref().is_none_or(|(_, time)| modified > *time) {
            latest = Some((entry.file_name().to_string_lossy().into_owned(), modified));
        }
    }
    latest
        .map(|(name, _)| name)
        .context("no sessions to continue")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_names() {
        assert!(clean_name("../bad").is_err());
        assert_eq!(clean_name("work").unwrap(), "work");
    }

    #[test]
    fn old_session_metadata_defaults_token_usage() {
        let meta: SessionMeta = serde_json::from_str(r#"{"name":"old","created_at":1}"#).unwrap();
        assert_eq!(meta.total_tokens, 0);
    }
}
