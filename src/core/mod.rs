pub mod state;

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{
    config::Config,
    project::ProjectState,
    protocol::{self, Action, CatalogEntry, CatalogSnapshot},
    provider::Provider,
    runtime::{self, Command, Event, FileContent, ImageContent},
    session::{self, Session, SessionMeta},
    tool,
};
use state::{Change, Projection, Snapshot};

#[derive(Clone, Debug, Serialize)]
pub struct Update {
    pub session_id: String,
    pub seq: u64,
    pub changes: Vec<Change>,
    #[serde(skip)]
    pub event: Event,
}

struct Hub {
    projection: Projection,
    updates: broadcast::Sender<Arc<Update>>,
}
struct Catalog {
    snapshot: CatalogSnapshot,
    updates: broadcast::Sender<CatalogSnapshot>,
}

pub struct Subscription {
    pub snapshot: Snapshot,
    pub updates: broadcast::Receiver<Arc<Update>>,
}

struct LoadedSession {
    commands: mpsc::Sender<Command>,
    hub: Arc<Mutex<Hub>>,
    directory: PathBuf,
    task: AsyncMutex<Option<JoinHandle<()>>>,
    pump: AsyncMutex<Option<JoinHandle<()>>>,
    _lock: session::SessionLock,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectUpdate {
    pub seq: u64,
    pub project: ProjectState,
}

struct Inner {
    config: Config,
    project_root: PathBuf,
    storage_root: PathBuf,
    provider: Arc<dyn Provider>,
    sessions: AsyncMutex<HashMap<String, Arc<LoadedSession>>>,
    catalog: Arc<Mutex<Catalog>>,
    project: watch::Receiver<ProjectUpdate>,
    refresh_project: mpsc::Sender<()>,
    project_task: AsyncMutex<Option<JoinHandle<()>>>,
    closing: AtomicBool,
}

#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
}

impl Core {
    pub async fn new(
        config: Config,
        project_root: PathBuf,
        storage_root: PathBuf,
        provider: Arc<dyn Provider>,
    ) -> Result<Self> {
        let project_root = tokio::fs::canonicalize(project_root).await?;
        tokio::fs::create_dir_all(&storage_root).await?;
        let mut entries = Vec::new();
        for info in Session::list_in(storage_root.clone()).await? {
            let meta: SessionMeta = serde_json::from_slice(
                &tokio::fs::read(storage_root.join(&info.name).join("session.json")).await?,
            )?;
            if meta
                .project_root
                .as_ref()
                .is_some_and(|root| *root != project_root)
            {
                continue;
            }
            entries.push(CatalogEntry {
                info,
                project_root: meta.project_root,
                total_tokens: meta.total_tokens,
                total_cost: (meta.cost_complete && meta.total_tokens > 0)
                    .then_some(meta.total_cost),
                activity: "idle".into(),
            });
        }
        let (updates, _) = broadcast::channel(64);
        let catalog = Arc::new(Mutex::new(Catalog {
            snapshot: CatalogSnapshot {
                seq: 0,
                sessions: entries,
            },
            updates,
        }));
        let initial = ProjectState::at(project_root.clone()).await?;
        let (project_tx, project) = watch::channel(ProjectUpdate {
            seq: 0,
            project: initial.clone(),
        });
        let (refresh_project, mut refreshes) = mpsc::channel(1);
        let project_task = tokio::spawn(async move {
            let mut project = initial;
            let mut seq = 0;
            while refreshes.recv().await.is_some() {
                project.refresh().await;
                project.git_diff.clear();
                project.git_diff_path = None;
                seq += 1;
                project_tx.send_replace(ProjectUpdate {
                    seq,
                    project: project.clone(),
                });
            }
        });
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                project_root,
                storage_root,
                provider,
                sessions: AsyncMutex::new(HashMap::new()),
                catalog,
                project,
                refresh_project,
                project_task: AsyncMutex::new(Some(project_task)),
                closing: AtomicBool::new(false),
            }),
        })
    }

    pub fn models(&self) -> &[crate::config::ModelConfig] {
        &self.inner.config.models
    }
    pub fn project_root(&self) -> &Path {
        &self.inner.project_root
    }
    pub fn subscribe_project(&self) -> watch::Receiver<ProjectUpdate> {
        self.inner.project.clone()
    }

    pub fn subscribe_catalog(&self) -> (CatalogSnapshot, broadcast::Receiver<CatalogSnapshot>) {
        let catalog = self.inner.catalog.lock().unwrap();
        (catalog.snapshot.clone(), catalog.updates.subscribe())
    }

    pub async fn open(&self, name: Option<String>) -> Result<String> {
        self.load(name, false).await
    }
    pub async fn create(&self, name: Option<String>) -> Result<String> {
        self.load(name, true).await
    }

    async fn load(&self, name: Option<String>, create: bool) -> Result<String> {
        let name = name.map(|name| session::clean_name(&name)).transpose()?;
        let mut sessions = self.inner.sessions.lock().await;
        if self.inner.closing.load(Ordering::Acquire) {
            bail!("core is shutting down");
        }
        if let Some(name) = &name
            && sessions.contains_key(name)
        {
            if create {
                bail!("session already exists: {name}");
            }
            return Ok(name.clone());
        }
        let existing = name
            .as_ref()
            .filter(|name| self.inner.storage_root.join(name).is_dir());
        let (mut session, messages, lock) = if let Some(name) = existing {
            if create {
                bail!("session already exists: {name}");
            }
            let directory = self.inner.storage_root.join(name);
            let lock = session::lock_session(&directory)?;
            let (session, messages) =
                Session::resume_in(self.inner.storage_root.clone(), name).await?;
            (session, messages, lock)
        } else {
            let mut session = Session::new_in(self.inner.storage_root.clone(), name).await?;
            let lock = session.take_creation_lock();
            (session, Vec::new(), lock)
        };
        if session
            .meta
            .project_root
            .as_ref()
            .is_some_and(|root| *root != self.inner.project_root)
        {
            bail!("session belongs to another project");
        }
        session.meta.project_root = Some(self.inner.project_root.clone());
        session.save().await?;
        let id = session.meta.name.clone();
        let directory = session.directory();
        {
            let mut catalog = self.inner.catalog.lock().unwrap();
            if !catalog
                .snapshot
                .sessions
                .iter()
                .any(|entry| entry.info.name == id)
            {
                catalog.snapshot.sessions.push(CatalogEntry {
                    info: session::SessionInfo {
                        name: id.clone(),
                        title: session.meta.title.clone(),
                        created_at: session.meta.created_at,
                        first_message: None,
                    },
                    project_root: session.meta.project_root.clone(),
                    total_tokens: session.meta.total_tokens,
                    total_cost: session.total_cost(),
                    activity: "idle".into(),
                });
            }
            catalog.snapshot.sessions.sort_by(|a, b| {
                b.info
                    .created_at
                    .cmp(&a.info.created_at)
                    .then(a.info.name.cmp(&b.info.name))
            });
            catalog.publish();
        }
        let tools = tool::discover_at(&self.inner.config, &self.inner.project_root).await?;
        let project = self.inner.project.borrow().project.clone();
        let (commands, mut events, task) = runtime::spawn_session(
            self.inner.config.clone(),
            Arc::new(crate::raw::RecordingProvider {
                inner: self.inner.provider.clone(),
                directory: directory.clone(),
            }),
            tools,
            session,
            messages,
            project,
        );
        let (updates, _) = broadcast::channel(256);
        let hub = Arc::new(Mutex::new(Hub {
            projection: Projection::new(id.clone()),
            updates,
        }));
        let (ready, initialized) = oneshot::channel();
        let pump_hub = hub.clone();
        let catalog = self.inner.catalog.clone();
        let refresh = self.inner.refresh_project.clone();
        let pump_id = id.clone();
        let image_directory = directory.clone();
        let bound_root = self.inner.project_root.clone();
        let pump = tokio::spawn(async move {
            let mut ready = Some(ready);
            while let Some(mut event) = events.recv().await {
                if let Event::Barrier(reply) = &event {
                    if let Some(reply) = reply.lock().unwrap().take() {
                        reply.send(()).ok();
                    }
                    continue;
                }
                if matches!(event, Event::Ready) {
                    crate::logging::write("INFO", &pump_id, "session ready");
                    if let Some(ready) = ready.take() {
                        ready.send(()).ok();
                    }
                    continue;
                }
                if matches!(event, Event::RefreshProject) {
                    refresh.try_send(()).ok();
                    continue;
                }
                if let Event::ToolImage { image, .. } = &mut event {
                    let saved = async {
                        let bytes = STANDARD.decode(&image.data)?;
                        session::store_attachment(&image_directory, &bytes).await
                    }
                    .await;
                    match saved {
                        Ok(stored) => *image = stored,
                        Err(error) => event = Event::Notice(format!("store tool image: {error:#}")),
                    }
                }
                let catalog_event = matches!(
                    event,
                    Event::SessionChanged(_)
                        | Event::UsageChanged { .. }
                        | Event::OperationStarted { .. }
                        | Event::ApprovalRequested { .. }
                        | Event::ApprovalResolved { .. }
                        | Event::GenerationFinished { .. }
                        | Event::GenerationCancelled
                        | Event::Error(_)
                        | Event::MessageAccepted(_)
                );
                let state = {
                    let mut hub = pump_hub.lock().unwrap();
                    let changes = hub.projection.apply(&event);
                    crate::logging::event(&event, &hub.projection.snapshot);
                    let state = hub.projection.snapshot.state.clone();
                    let seq = hub.projection.snapshot.seq;
                    hub.updates
                        .send(Arc::new(Update {
                            session_id: pump_id.clone(),
                            seq,
                            changes,
                            event: event.clone(),
                        }))
                        .ok();
                    state
                };
                if catalog_event {
                    let mut catalog = catalog.lock().unwrap();
                    if let Some(entry) = catalog
                        .snapshot
                        .sessions
                        .iter_mut()
                        .find(|entry| entry.info.name == pump_id)
                    {
                        if state.title != pump_id {
                            entry.info.title = Some(state.title);
                        }
                        entry.project_root = Some(bound_root.clone());
                        entry.total_tokens = state.total_tokens;
                        entry.total_cost = state.total_cost;
                        entry.activity = if state.approval.is_some() {
                            "approval"
                        } else if state.turn_id.is_some() {
                            "running"
                        } else {
                            "idle"
                        }
                        .into();
                        if let Event::MessageAccepted(crate::runtime::Message::User {
                            content, ..
                        }) = &event
                            && entry.info.first_message.is_none()
                        {
                            entry.info.first_message = Some(content.clone());
                        }
                    }
                    catalog.publish();
                }
            }
        });
        initialized
            .await
            .context("session stopped during initialization")?;
        sessions.insert(
            id.clone(),
            Arc::new(LoadedSession {
                commands,
                hub,
                directory,
                task: AsyncMutex::new(Some(task)),
                pump: AsyncMutex::new(Some(pump)),
                _lock: lock,
            }),
        );
        Ok(id)
    }

    async fn session(&self, id: &str) -> Result<Arc<LoadedSession>> {
        if self.inner.closing.load(Ordering::Acquire) {
            bail!("core is shutting down");
        }
        let id = session::clean_name(id)?;
        if let Some(session) = self.inner.sessions.lock().await.get(&id).cloned() {
            return Ok(session);
        }
        if !self.inner.storage_root.join(&id).is_dir() {
            bail!("unknown session: {id}");
        }
        self.open(Some(id.clone())).await?;
        self.inner
            .sessions
            .lock()
            .await
            .get(&id)
            .cloned()
            .context("session is unavailable")
    }

    pub async fn subscribe(&self, id: &str) -> Result<Subscription> {
        let session = self.session(id).await?;
        let hub = session.hub.lock().unwrap();
        let mut snapshot = hub.projection.snapshot();
        snapshot.project = self.inner.project.borrow().project.clone();
        Ok(Subscription {
            snapshot,
            updates: hub.updates.subscribe(),
        })
    }

    pub async fn raw_request(
        &self,
        id: &str,
        block_id: &str,
    ) -> protocol::Result<serde_json::Value> {
        let session = self.session(id).await.map_err(core_error)?;
        let request_id = {
            let hub = session.hub.lock().unwrap();
            hub.projection.snapshot.blocks.iter().find(|block| block.id == block_id)
                .ok_or_else(|| protocol::Error::new("not_found", "unknown block"))?
                .raw_request.clone()
                .ok_or_else(|| protocol::Error::new("not_found", "No recorded model request at this point. Older conversations have no raw data."))?
        };
        let request_id = uuid::Uuid::parse_str(&request_id).map_err(|e| core_error(e.into()))?;
        let bytes = tokio::fs::read(
            session
                .directory
                .join("requests")
                .join(format!("{request_id}.json")),
        )
        .await
        .map_err(|e| core_error(e.into()))?;
        serde_json::from_slice(&bytes).map_err(|e| core_error(e.into()))
    }

    pub async fn command(&self, id: &str, mut action: Action) -> protocol::Result<protocol::Accepted> {
        let session = self.session(id).await.map_err(core_error)?;
        let mut images = Vec::new();
        if let Action::SendMessage {
            content,
            attachments,
        } = &mut action
        {
            if content.len() > protocol::MAX_COMMAND_BYTES || attachments.len() > 8 {
                return Err(protocol::Error::new(
                    "too_large",
                    "message exceeds the command or attachment limit",
                ));
            }
            for id in attachments {
                if id.starts_with("uploads/") {
                    let file = session::load_file_upload(&session.directory, id).await.map_err(core_error)?;
                    content.push_str("\n\n");
                    content.push_str(&file.prompt);
                    continue;
                }
                images.push(
                    session::load_attachment(&session.directory, id)
                        .await
                        .map_err(core_error)?,
                );
            }
        }
        let (reply, result) = oneshot::channel();
        let (published, publication) = oneshot::channel();
        session
            .commands
            .send(Command::Request {
                action,
                images,
                reply,
                published,
            })
            .await
            .map_err(|_| protocol::Error::new("closed", "session stopped"))?;
        let result = result
            .await
            .map_err(|_| protocol::Error::new("closed", "session stopped before replying"))??;
        publication
            .await
            .map_err(|_| protocol::Error::new("closed", "session stopped before publishing"))?;
        Ok(result)
    }

    pub async fn attach(&self, id: &str, bytes: &[u8]) -> Result<ImageContent> {
        let session = self.session(id).await?;
        session::store_attachment(&session.directory, bytes).await
    }

    /// The full, un-redacted view of one transcript block, for connections
    /// that were only receiving its collapsed form.
    pub async fn block(&self, id: &str, block_id: &str) -> protocol::Result<state::Block> {
        let session = self.session(id).await.map_err(core_error)?;
        let hub = session.hub.lock().unwrap();
        hub.projection
            .snapshot
            .blocks
            .iter()
            .find(|block| block.id == block_id)
            .cloned()
            .ok_or_else(|| {
                protocol::Error::new("unknown_block", format!("unknown block: {block_id}"))
            })
    }

    /// Permanently removes a session: transcript, attachments, and metadata.
    /// A live session must be idle; a session owned by another process is
    /// refused while its writer lock is held.
    pub async fn delete(&self, id: &str) -> protocol::Result<()> {
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(protocol::Error::new("closed", "core is shutting down"));
        }
        let id = session::clean_name(id).map_err(core_error)?;
        let directory = self.inner.storage_root.join(&id);
        let unknown = || {
            protocol::Error::new("unknown", format!("unknown session: {id}"))
        };
        let bytes = tokio::fs::read(directory.join("session.json")).await.map_err(|_| unknown())?;
        let meta: session::SessionMeta = serde_json::from_slice(&bytes).map_err(|_| unknown())?;
        if meta.project_root.as_ref().is_some_and(|root| root != &self.inner.project_root) {
            return Err(protocol::Error::new("unknown", format!("unknown session: {id}")));
        }
        {
            let catalog = self.inner.catalog.lock().unwrap();
            if let Some(entry) = catalog
                .snapshot
                .sessions
                .iter()
                .find(|entry| entry.info.name == id)
            {
                if entry.activity != "idle" {
                    return Err(protocol::Error::new(
                        "busy",
                        "finish or cancel the active operation before deleting the session",
                    ));
                }
            }
        }
        let loaded = self.inner.sessions.lock().await.remove(&id);
        // A loaded session holds its writer lock until it drops; an unloaded
        // one must be locked here so a concurrent writer cannot resurrect it
        // between the check and the removal. Both stay held across the
        // directory deletion.
        let lock = match &loaded {
            Some(_) => None,
            None => Some(
                session::lock_session(&directory).map_err(|error| {
                    protocol::Error::new(
                        "busy",
                        format!("session is owned by another process: {error:#}"),
                    )
                })?,
            ),
        };
        if let Some(loaded) = loaded.as_ref() {
            if let Some(task) = loaded.task.lock().await.take() {
                let (reply, result) = oneshot::channel();
                loaded
                    .commands
                    .send(Command::Shutdown(reply))
                    .await
                    .map_err(|_| {
                        protocol::Error::new("closed", "session stopped before shutdown")
                    })?;
                let summary = result
                    .await
                    .map_err(|_| protocol::Error::new("closed", "session stopped"))?;
                if let Some(error) = summary.error {
                    return Err(protocol::Error::new("shutdown", error));
                }
                if let Err(error) = task.await {
                    return Err(protocol::Error::new(
                        "shutdown",
                        format!("session stopped: {error:#}"),
                    ));
                }
            }
            if let Some(pump) = loaded.pump.lock().await.take() {
                pump.await.ok();
            }
        }
        if let Err(error) = tokio::fs::remove_dir_all(&directory).await {
            return Err(protocol::Error::new(
                "core",
                format!("remove session directory: {error:#}"),
            ));
        }
        drop(lock);
        drop(loaded);
        let mut catalog = self.inner.catalog.lock().unwrap();
        let before = catalog.snapshot.sessions.len();
        catalog.snapshot.sessions.retain(|entry| entry.info.name != id);
        if catalog.snapshot.sessions.len() != before {
            catalog.publish();
        }
        Ok(())
    }

    pub async fn upload(&self, id: &str, name: &str, bytes: &[u8]) -> Result<serde_json::Value> {
        if image::guess_format(bytes).is_ok() {
            return Ok(serde_json::to_value(self.attach(id, bytes).await?)?);
        }
        let session = self.session(id).await?;
        Ok(serde_json::to_value(session::store_file_upload(&session.directory, name, bytes.to_vec()).await?)?)
    }

    pub async fn attachment(&self, id: &str, attachment: &str) -> Result<ImageContent> {
        let session = self.session(id).await?;
        session::load_attachment(&session.directory, attachment).await
    }

    pub async fn published_file(&self, id: &str, block_id: &str) -> Result<FileContent> {
        let session = self.session(id).await?;
        let hub = session.hub.lock().unwrap();
        hub.projection
            .snapshot
            .blocks
            .iter()
            .find(|block| block.id == block_id)
            .and_then(|block| block.file.clone())
            .context("unknown published file")
    }

    pub async fn diff(&self, path: Option<&Path>) -> Result<String> {
        if let Some(path) = path
            && (path.is_absolute()
                || path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_))))
        {
            bail!("diff path must be relative to the project");
        }
        crate::project::diff(&self.inner.project_root, path).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.inner.closing.store(true, Ordering::Release);
        let sessions = self
            .inner
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let results = futures_util::future::join_all(sessions.iter().map(|session| async move {
            let (reply, result) = oneshot::channel();
            session
                .commands
                .send(Command::Shutdown(reply))
                .await
                .context("session stopped before shutdown")?;
            let summary = result.await.context("session stopped during shutdown")?;
            if let Some(task) = session.task.lock().await.take() {
                task.await?;
            }
            if let Some(pump) = session.pump.lock().await.take() {
                pump.await?;
            }
            if let Some(error) = summary.error {
                bail!(error);
            }
            Ok::<_, anyhow::Error>(())
        }))
        .await;
        self.inner.sessions.lock().await.clear();
        if let Some(task) = self.inner.project_task.lock().await.take() {
            task.abort();
            task.await.ok();
        }
        let errors = results
            .into_iter()
            .filter_map(Result::err)
            .map(|e| format!("{e:#}"))
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        Ok(())
    }
}

impl Catalog {
    fn publish(&mut self) {
        self.snapshot.seq += 1;
        self.updates.send(self.snapshot.clone()).ok();
    }
}

fn core_error(error: anyhow::Error) -> protocol::Error {
    protocol::Error::new("core", format!("{error:#}"))
}
