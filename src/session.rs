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
    pub total_cost: f64,
    #[serde(default)]
    pub cost_complete: bool,
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default)]
    pub compaction_summary: Option<String>,
    #[serde(default)]
    pub compacted_through: usize,
    #[serde(default)]
    pub approved_tools: Vec<String>,
}

pub struct Session {
    root: PathBuf,
    pub meta: SessionMeta,
}

/// One row of the session picker: the persisted identity of a session plus
/// the first user message, which summarizes sessions without a title.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub name: String,
    pub title: Option<String>,
    pub created_at: u64,
    pub first_message: Option<String>,
}

impl SessionInfo {
    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

impl Session {
    pub async fn open(startup: Startup) -> Result<(Self, Vec<Message>)> {
        let root = sessions_root()?;
        tokio::fs::create_dir_all(&root).await?;
        let Some(name) = startup.session else {
            let session = Self::create(root, auto_name()).await?;
            return Ok((session, Vec::new()));
        };
        let name = clean_name(&name)?;
        if root.join(&name).is_dir() {
            return Self::load(root, name).await;
        }
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

    /// Every persisted session, most recent first.
    pub async fn list() -> Result<Vec<SessionInfo>> {
        Self::list_in(sessions_root()?).await
    }

    pub async fn list_in(root: PathBuf) -> Result<Vec<SessionInfo>> {
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Ok(Vec::new());
        };
        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Ok(data) = tokio::fs::read(path.join("session.json")).await else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<SessionMeta>(&data) else {
                continue;
            };
            sessions.push(SessionInfo {
                name: meta.name,
                title: meta.title,
                created_at: meta.created_at,
                first_message: first_user_message(&path.join("messages.jsonl")).await,
            });
        }
        sessions.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(sessions)
    }

    /// Loads a persisted session by name for resumption.
    pub async fn resume(name: &str) -> Result<(Self, Vec<Message>)> {
        Self::resume_in(sessions_root()?, name).await
    }

    pub async fn resume_in(root: PathBuf, name: &str) -> Result<(Self, Vec<Message>)> {
        let name = clean_name(name)?;
        if !root.join(&name).is_dir() {
            bail!("unknown session: {name}");
        }
        Self::load(root, name).await
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
                total_cost: 0.0,
                cost_complete: true,
                context_tokens: 0,
                compaction_summary: None,
                compacted_through: 0,
                approved_tools: Vec::new(),
            },
        };
        session.save().await?;
        tokio::fs::write(session.messages_path(), []).await?;
        Ok(session)
    }

    async fn load(root: PathBuf, name: String) -> Result<(Self, Vec<Message>)> {
        let directory = root.join(&name);
        let mut meta: SessionMeta = serde_json::from_slice(
            &tokio::fs::read(directory.join("session.json"))
                .await
                .with_context(|| format!("load session {name}"))?,
        )?;
        for tool in &mut meta.approved_tools {
            match tool.as_str() {
                "grep" => *tool = "search_files".into(),
                "glob" => *tool = "list_files".into(),
                _ => {}
            }
        }
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

    pub fn record_usage(&mut self, tokens: u64, price_per_token: Option<f64>) {
        self.meta.total_tokens += tokens;
        if self.meta.cost_complete {
            if let Some(price) = price_per_token {
                self.meta.total_cost += tokens as f64 * price;
            } else {
                self.meta.cost_complete = false;
            }
        }
    }

    pub fn total_cost(&self) -> Option<f64> {
        (self.meta.cost_complete && self.meta.total_tokens > 0).then_some(self.meta.total_cost)
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

async fn first_user_message(path: &Path) -> Option<String> {
    let data = tokio::fs::read_to_string(path).await.ok()?;
    data.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<Message>(line) {
            Ok(Message::User { content, .. }) if !content.trim().is_empty() => Some(content),
            _ => None,
        })
        .next()
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
        assert_eq!(meta.total_cost, 0.0);
        assert!(!meta.cost_complete);
        assert!(meta.title.is_none());
        assert!(meta.plan.is_none());
        assert!(meta.approved_tools.is_empty());
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
                total_cost: 0.0,
                cost_complete: true,
                context_tokens: 0,
                compaction_summary: None,
                compacted_through: 0,
                approved_tools: Vec::new(),
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

    #[test]
    fn session_cost_requires_a_price_for_every_usage() {
        let mut session = Session {
            root: PathBuf::new(),
            meta: SessionMeta {
                name: "priced".into(),
                title: None,
                plan: None,
                created_at: 1,
                total_tokens: 0,
                total_cost: 0.0,
                cost_complete: true,
                context_tokens: 0,
                compaction_summary: None,
                compacted_through: 0,
                approved_tools: Vec::new(),
            },
        };

        assert_eq!(session.total_cost(), None);
        session.record_usage(100, Some(0.01));
        assert_eq!(session.total_cost(), Some(1.0));

        session.record_usage(25, None);
        assert_eq!(session.total_cost(), None);
        session.record_usage(10, Some(0.01));
        assert_eq!(session.total_cost(), None);
        assert_eq!(session.meta.total_tokens, 135);
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

    #[tokio::test]
    async fn session_tool_approvals_survive_reload() {
        let root = std::env::temp_dir().join(format!(
            "rope-approval-session-test-{}-{}",
            std::process::id(),
            now()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let mut session = Session::create(root.clone(), "approvals".into())
            .await
            .unwrap();
        session.meta.approved_tools.push("shell".into());
        session.meta.approved_tools.push("grep".into());
        session.meta.approved_tools.push("glob".into());
        session.save().await.unwrap();

        let (loaded, _) = Session::load(root.clone(), "approvals".into())
            .await
            .unwrap();

        assert_eq!(
            loaded.meta.approved_tools,
            ["shell", "search_files", "list_files"]
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn list_sessions_orders_by_recency_and_extracts_summaries() {
        let root = std::env::temp_dir().join(format!(
            "rope-session-list-test-{}-{}",
            std::process::id(),
            now()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();

        let mut old = Session::create(root.clone(), "older".into()).await.unwrap();
        old.meta.created_at = 1_000;
        old.save().await.unwrap();
        tokio::fs::write(
            root.join("older").join("messages.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&Message::user_with_images(
                    "summarize the crash log".into(),
                    Vec::new()
                ))
                .unwrap()
            ),
        )
        .await
        .unwrap();

        let mut new = Session::create(root.clone(), "newer".into()).await.unwrap();
        new.meta.created_at = 2_000;
        new.set_title("Git Pane Scrolling".into());
        new.save().await.unwrap();

        // A stray file and a directory without valid metadata are skipped.
        tokio::fs::write(root.join("stray.txt"), "not a session")
            .await
            .unwrap();
        let broken = root.join("broken");
        tokio::fs::create_dir(&broken).await.unwrap();
        tokio::fs::write(broken.join("session.json"), "{not json")
            .await
            .unwrap();

        let sessions = Session::list_in(root.clone()).await.unwrap();

        assert_eq!(
            sessions
                .iter()
                .map(|info| info.name.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert_eq!(sessions[0].title.as_deref(), Some("Git Pane Scrolling"));
        assert_eq!(sessions[0].first_message, None);
        assert_eq!(
            sessions[1].first_message.as_deref(),
            Some("summarize the crash log")
        );
        assert_eq!(sessions[1].display_name(), "older");

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn list_sessions_is_empty_before_any_session_exists() {
        let root = std::env::temp_dir().join(format!(
            "rope-session-empty-list-test-{}-{}",
            std::process::id(),
            now()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();

        assert!(Session::list_in(root.clone()).await.unwrap().is_empty());

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn resume_loads_a_persisted_session_by_name() {
        let root = std::env::temp_dir().join(format!(
            "rope-session-resume-test-{}-{}",
            std::process::id(),
            now()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create(root.clone(), "work".into()).await.unwrap();
        session
            .append(&[Message::user_with_images("keep going".into(), Vec::new())])
            .await
            .unwrap();

        let (loaded, messages) = Session::resume_in(root.clone(), "work").await.unwrap();
        assert_eq!(loaded.meta.name, "work");
        assert_eq!(
            messages,
            vec![Message::user_with_images("keep going".into(), Vec::new())]
        );

        assert!(Session::resume_in(root.clone(), "missing").await.is_err());
        assert!(Session::resume_in(root.clone(), "../work").await.is_err());

        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
