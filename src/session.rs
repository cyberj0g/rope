use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{config::Startup, runtime::Message, tool::ExecutionPlan};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionMeta {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub plan: Option<ExecutionPlan>,
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
                title: None,
                plan: None,
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
        let mut messages = data
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for message in &mut messages {
            hydrate_images(&directory, message).await?;
        }
        Ok((Self { root, meta }, messages))
    }

    pub async fn append(&self, messages: &[Message]) -> Result<()> {
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(self.messages_path())
            .await?;
        for (index, message) in messages.iter().enumerate() {
            let mut stored = message.clone();
            persist_images(&self.directory(), &mut stored, index).await?;
            file.write_all(&serde_json::to_vec(&stored)?).await?;
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

    pub fn display_name(&self) -> &str {
        self.meta.title.as_deref().unwrap_or(&self.meta.name)
    }

    pub fn needs_title(&self) -> bool {
        self.meta.title.is_none() && is_auto_name(&self.meta.name)
    }

    pub fn set_title(&mut self, title: String) {
        self.meta.title = Some(title);
    }

    fn directory(&self) -> PathBuf {
        self.root.join(&self.meta.name)
    }
    fn messages_path(&self) -> PathBuf {
        self.directory().join("messages.jsonl")
    }
}

async fn persist_images(directory: &Path, message: &mut Message, index: usize) -> Result<()> {
    let images = message_images_mut(message);
    if images.is_empty() {
        return Ok(());
    }
    let attachments = directory.join("attachments");
    tokio::fs::create_dir_all(&attachments).await?;
    for (image_index, image) in images.into_iter().enumerate() {
        if image.path.is_some() {
            continue;
        }
        let extension = match image.mime_type.as_str() {
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "png",
        };
        let name = format!(
            "{}-{}-{index}-{image_index}.{extension}",
            now(),
            std::process::id()
        );
        let relative = format!("attachments/{name}");
        let bytes = STANDARD
            .decode(&image.data)
            .context("decode image attachment")?;
        tokio::fs::write(directory.join(&relative), bytes).await?;
        image.path = Some(relative);
    }
    Ok(())
}

async fn hydrate_images(directory: &Path, message: &mut Message) -> Result<()> {
    for image in message_images_mut(message) {
        if !image.data.is_empty() {
            continue;
        }
        let path = image
            .path
            .as_deref()
            .context("image attachment has no path")?;
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("invalid image attachment path: {path}");
        }
        image.data = STANDARD.encode(
            tokio::fs::read(directory.join(relative))
                .await
                .with_context(|| format!("read image attachment {path}"))?,
        );
    }
    Ok(())
}

fn message_images_mut(message: &mut Message) -> Vec<&mut crate::runtime::ImageContent> {
    match message {
        Message::User { images, .. } => images.iter_mut().collect(),
        Message::Tool {
            image: Some(image), ..
        } => vec![image],
        _ => Vec::new(),
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

fn is_auto_name(name: &str) -> bool {
    name.strip_prefix("session-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.chars().all(|char| char.is_ascii_digit())
    })
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
        assert!(meta.title.is_none());
        assert!(meta.plan.is_none());
    }

    #[test]
    fn generated_titles_replace_only_automatic_names_for_display() {
        let mut session = Session {
            root: PathBuf::new(),
            meta: SessionMeta {
                name: "session-123".into(),
                title: None,
                plan: None,
                created_at: 1,
                total_tokens: 0,
                context_tokens: 0,
                compaction_summary: None,
                compacted_through: 0,
            },
        };
        assert!(session.needs_title());
        assert_eq!(session.display_name(), "session-123");

        session.set_title("Git Pane Scrolling".into());
        assert!(!session.needs_title());
        assert_eq!(session.display_name(), "Git Pane Scrolling");

        session.meta.name = "named-session".into();
        session.meta.title = None;
        assert!(!session.needs_title());
    }

    #[tokio::test]
    async fn image_data_is_stored_as_a_session_attachment() {
        let root = std::env::temp_dir().join(format!(
            "rope-image-session-test-{}-{}",
            std::process::id(),
            now()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create(root.clone(), "images".into())
            .await
            .unwrap();
        let message = Message::user_with_images(
            "look".into(),
            vec![crate::runtime::ImageContent {
                mime_type: "image/png".into(),
                data: STANDARD.encode(b"png bytes"),
                path: None,
                width: 2,
                height: 3,
            }],
        );

        session.append(&[message]).await.unwrap();
        let stored = tokio::fs::read_to_string(session.messages_path())
            .await
            .unwrap();
        assert!(!stored.contains(&STANDARD.encode(b"png bytes")));
        assert!(stored.contains("attachments/"));

        let (_, messages) = Session::load(root.clone(), "images".into()).await.unwrap();
        assert!(matches!(
            &messages[0],
            Message::User { images, .. } if images[0].data == STANDARD.encode(b"png bytes")
        ));

        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
