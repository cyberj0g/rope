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
    agent::{AgentCatalog, MAX_DELEGATION_DEPTH},
    config::Config,
    project::ProjectState,
    protocol::{self, Action, CatalogEntry, CatalogSnapshot},
    provider::Provider,
    runtime::{self, Command, Event, FileContent, ImageContent, SubagentOutcome, SubagentStatus},
    session::{self, ParentRef, Session, SessionMeta, SessionSettings},
    tool::{self, DelegationCommand, DelegationPort, SubagentTool},
};
use state::{Change, ChildLink, Projection, Snapshot};

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
    /// Delegation depth: 0 for root sessions, one more than the parent's.
    depth: u8,
}

/// One in-flight `subagent` invocation, keyed by the parent session.
#[derive(Clone)]
struct ActiveDelegation {
    parent_turn: String,
    tool_call_id: String,
    child: String,
    /// The child's delegated turn, known once its submit was accepted.
    child_turn: Option<String>,
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
    loading: AsyncMutex<()>,
    catalog: Arc<Mutex<Catalog>>,
    project: watch::Receiver<ProjectUpdate>,
    refresh_project: mpsc::Sender<()>,
    project_task: AsyncMutex<Option<JoinHandle<()>>>,
    closing: AtomicBool,
    /// The resolved agent definitions, loaded once at startup.
    agents: Arc<AgentCatalog>,
    /// The runtime side of the delegation port. Taken (and dropped) on
    /// shutdown so the loop's receiver closes and the loop ends.
    delegation_tx: std::sync::Mutex<Option<DelegationPort>>,
    /// In-flight `subagent` invocations, keyed by the parent session.
    delegations: AsyncMutex<HashMap<String, ActiveDelegation>>,
    /// Serializes steering-route decisions against delegation settlement.
    route_lock: AsyncMutex<()>,
    /// The task driving the delegation port; drained on shutdown.
    delegation_task: std::sync::Mutex<Option<JoinHandle<()>>>,
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
        // Agents are loaded once at startup; a malformed file fails boot
        // with the file and field named.
        let agents = Arc::new(AgentCatalog::load(&config, &project_root)?);
        let mut entries = Vec::new();
        for info in Session::list_in(storage_root.clone()).await? {
            let meta: SessionMeta = serde_json::from_slice(
                &tokio::fs::read(storage_root.join(&info.name).join("session.json")).await?,
            )?;
            // Child sessions stay out of the root catalog; they are
            // reached through their parent.
            if meta.parent.is_some() {
                continue;
            }
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
        let (delegation_tx, delegation_rx) = mpsc::unbounded_channel();
        let core = Self {
            inner: Arc::new(Inner {
                config,
                project_root,
                storage_root,
                provider,
                sessions: AsyncMutex::new(HashMap::new()),
                loading: AsyncMutex::new(()),
                catalog,
                project,
                refresh_project,
                project_task: AsyncMutex::new(Some(project_task)),
                closing: AtomicBool::new(false),
                agents,
                delegation_tx: std::sync::Mutex::new(Some(delegation_tx)),
                delegations: AsyncMutex::new(HashMap::new()),
                route_lock: AsyncMutex::new(()),
                delegation_task: std::sync::Mutex::new(None),
            }),
        };
        let task = tokio::spawn(delegation_loop(core.clone(), delegation_rx));
        *core.inner.delegation_task.lock().unwrap() = Some(task);
        Ok(core)
    }

    pub fn models(&self) -> &[crate::config::ModelConfig] {
        &self.inner.config.models
    }
    pub fn agents(&self) -> Arc<AgentCatalog> {
        self.inner.agents.clone()
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
        let _loading = self.inner.loading.lock().await;
        if self.inner.closing.load(Ordering::Acquire) {
            bail!("core is shutting down");
        }
        {
            let sessions = self.inner.sessions.lock().await;
            if let Some(name) = &name
                && sessions.contains_key(name)
            {
                if create {
                    bail!("session already exists: {name}");
                }
                return Ok(name.clone());
            }
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
        let is_child = session.meta.parent.is_some();
        let depth = session.meta.depth;
        if !is_child {
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
        let mut tools = tool::discover_at(&self.inner.config, &self.inner.project_root).await?;
        // The delegation tool is registered once per session; each agent's
        // schema advertises it only when that agent may delegate.
        if !self.inner.agents.delegable().is_empty() {
            tools.insert(
                SubagentTool::new(self.inner.agents.delegable()),
                self.inner.config.tools.subagent,
            );
        }
        let project = self.inner.project.borrow().project.clone();
        let delegation = self
            .inner
            .delegation_tx
            .lock()
            .unwrap()
            .clone()
            .context("core is shutting down")?;
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
            self.inner.agents.clone(),
            delegation,
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
                            let first = crate::runtime::strip_runtime_context(content);
                            if !first.is_empty() {
                                entry.info.first_message = Some(first.to_owned());
                            }
                        }
                    }
                    catalog.publish();
                }
            }
        });
        initialized
            .await
            .context("session stopped during initialization")?;
        let loaded = Arc::new(LoadedSession {
            commands,
            hub,
            directory,
            task: AsyncMutex::new(Some(task)),
            pump: AsyncMutex::new(Some(pump)),
            _lock: lock,
            depth,
        });
        let mut sessions = self.inner.sessions.lock().await;
        if self.inner.closing.load(Ordering::Acquire) {
            drop(sessions);
            let (reply, result) = oneshot::channel();
            loaded.commands.send(Command::Shutdown(reply)).await.ok();
            result.await.ok();
            if let Some(task) = loaded.task.lock().await.take() {
                task.await.ok();
            }
            if let Some(pump) = loaded.pump.lock().await.take() {
                pump.await.ok();
            }
            bail!("core is shutting down");
        }
        sessions.insert(id.clone(), loaded);
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
        // A projection rebuilt from history carries no delegation state:
        // restore the child links from the persisted records (and the
        // active child, if one is in flight) before the snapshot goes out.
        let active = self
            .inner
            .delegations
            .lock()
            .await
            .get(id)
            .map(|entry| entry.child.clone());
        let mut hub = session.hub.lock().unwrap();
        let mut links = self.child_links(&session, active.as_deref());
        for link in &mut links {
            if link.status == "unknown"
                && let Some(previous) = hub
                    .projection
                    .snapshot
                    .state
                    .children
                    .iter()
                    .find(|previous| previous.session == link.session)
            {
                link.status.clone_from(&previous.status);
            }
        }
        if hub.projection.snapshot.state.delegation != active
            || hub.projection.snapshot.state.children != links
        {
            let event = Event::DelegationChanged {
                child: active,
                children: links,
            };
            let changes = hub.projection.apply(&event);
            let seq = hub.projection.snapshot.seq;
            hub.updates
                .send(Arc::new(Update {
                    session_id: id.to_owned(),
                    seq,
                    changes,
                    event,
                }))
                .ok();
        }
        let mut snapshot = hub.projection.snapshot();
        snapshot.parent = std::fs::read(session.directory.join("session.json"))
            .ok()
            .and_then(|data| serde_json::from_slice::<SessionMeta>(&data).ok())
            .and_then(|meta| meta.parent);
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

    pub async fn command(
        &self,
        id: &str,
        mut action: Action,
    ) -> protocol::Result<protocol::Accepted> {
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
                    let file = session::load_file_upload(&session.directory, id)
                        .await
                        .map_err(core_error)?;
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
        // Routing decisions are serialized against delegation settlement,
        // so a steer is either addressed to the still-active descendant or
        // to the session itself — never both.
        let route_guard = if matches!(action, Action::SendMessage { .. }) {
            Some(self.inner.route_lock.lock().await)
        } else {
            None
        };
        let (target, receipt) = if matches!(action, Action::SendMessage { .. }) {
            let destination = self.route_steers(id).await?;
            match destination {
                Some(destination) => (destination.clone(), Some((id.to_owned(), destination))),
                None => (id.to_owned(), None),
            }
        } else if matches!(action, Action::Cancel { .. }) {
            // Cancel the descendants (deepest first) before the viewed
            // session, so each waiting call settles with user_cancelled.
            self.cancel_descendants(id).await?;
            (id.to_owned(), None)
        } else {
            (id.to_owned(), None)
        };
        let target_session = if target == id {
            session.clone()
        } else {
            self.session(&target).await.map_err(core_error)?
        };
        let mut accepted = self.send_command(&target_session, images, action).await?;
        drop(route_guard);
        if let Some((origin, destination)) = receipt {
            accepted.routed_to = Some(destination.clone());
            self.note_steer_receipt(&origin, &destination).await;
        }
        Ok(accepted)
    }

    async fn send_command(
        &self,
        session: &Arc<LoadedSession>,
        images: Vec<ImageContent>,
        action: Action,
    ) -> protocol::Result<protocol::Accepted> {
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

    /// The descendant a message to `id` should steer: follow the active
    /// delegation chain while the current node still runs the delegated
    /// turn. `None` means the session handles the message normally.
    async fn route_steers(&self, id: &str) -> protocol::Result<Option<String>> {
        let mut current = id.to_owned();
        let mut destination = None;
        loop {
            let Some(child) = self
                .inner
                .delegations
                .lock()
                .await
                .get(&current)
                .map(|entry| entry.child.clone())
            else {
                break;
            };
            // The child must still be running its delegated turn; a
            // settled child means the parent's normal send applies.
            let Ok(loaded) = self.session(&child).await else {
                break;
            };
            let running = {
                let hub = loaded.hub.lock().unwrap();
                hub.projection.snapshot.state.turn_id.is_some()
            };
            if !running {
                break;
            }
            destination = Some(child.clone());
            current = child;
        }
        Ok(destination)
    }

    /// Cancels the active descendants of `id`, deepest first, leaving `id`
    /// itself to its own cancel command.
    async fn cancel_descendants(&self, id: &str) -> protocol::Result<()> {
        let chain = {
            let _route = self.inner.route_lock.lock().await;
            let delegations = self.inner.delegations.lock().await;
            let mut chain = Vec::new();
            let mut current = id.to_owned();
            while let Some(entry) = delegations.get(&current) {
                chain.push((entry.child.clone(), entry.child_turn.clone()));
                current.clone_from(&entry.child);
            }
            chain
        };
        for (child, child_turn) in chain.into_iter().rev() {
            if let Some(turn) = child_turn
                && let Ok(session) = self.session(&child).await
            {
                let _ = self
                    .send_command(&session, Vec::new(), Action::Cancel { turn_id: turn })
                    .await;
            }
        }
        Ok(())
    }

    /// Records the forwarded-steer receipt in the origin transcript, so
    /// the parent sees where its input went when it resumes.
    async fn note_steer_receipt(&self, origin: &str, destination: &str) {
        let Ok(loaded) = self.session(origin).await else {
            return;
        };
        let agent = self
            .inner
            .agents
            .get(&self.destination_agent(destination).await)
            .map(|agent| agent.display_name())
            .unwrap_or_else(|| destination.to_owned());
        let event = Event::SteerReceipt {
            to: destination.to_owned(),
            agent,
        };
        let mut hub = loaded.hub.lock().unwrap();
        let changes = hub.projection.apply(&event);
        let seq = hub.projection.snapshot.seq;
        hub.updates
            .send(Arc::new(Update {
                session_id: origin.to_owned(),
                seq,
                changes,
                event,
            }))
            .ok();
    }

    /// The agent ID a destination child is running on.
    async fn destination_agent(&self, destination: &str) -> String {
        let path = self
            .inner
            .storage_root
            .join(destination)
            .join("session.json");
        let Ok(bytes) = std::fs::read(path) else {
            return crate::agent::ASSISTANT_ID.to_owned();
        };
        let Ok(meta) = serde_json::from_slice::<SessionMeta>(&bytes) else {
            return crate::agent::ASSISTANT_ID.to_owned();
        };
        meta.parent
            .as_ref()
            .map(|parent| parent.agent.clone())
            .or_else(|| meta.settings.as_ref().and_then(|s| s.agent.clone()))
            .unwrap_or_else(|| crate::agent::ASSISTANT_ID.to_owned())
    }

    /// Creates the child session, links both parents' records, submits the
    /// task, and resolves the reply exactly once when the child settles.
    async fn handle_delegation(&self, request: crate::tool::DelegationRequest) {
        let fail = |agent: &str, status: SubagentStatus, message: String| SubagentOutcome {
            session_id: None,
            agent: agent.to_owned(),
            status,
            response: None,
            error: None,
            message: Some(message),
            tokens: None,
        };
        // The agent must exist and be delegable; the runtime already
        // enforced the schema, and this recheck covers direct calls.
        let Some(agent) = self
            .inner
            .agents
            .get(&request.agent)
            .filter(|candidate| candidate.delegable())
        else {
            request
                .reply
                .send(fail(
                    &request.agent,
                    SubagentStatus::Failed,
                    "agent is not available for delegation".to_owned(),
                ))
                .ok();
            return;
        };
        let Ok(parent) = self.session(&request.parent_session).await else {
            request
                .reply
                .send(fail(
                    &request.agent,
                    SubagentStatus::Failed,
                    "parent session is unavailable".to_owned(),
                ))
                .ok();
            return;
        };
        if parent.depth.saturating_add(1) > MAX_DELEGATION_DEPTH {
            request
                .reply
                .send(fail(
                    &request.agent,
                    SubagentStatus::Failed,
                    format!("delegation depth limit of {MAX_DELEGATION_DEPTH} reached"),
                ))
                .ok();
            return;
        }
        let child_name = format!("sub-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        // Persist the parent's reciprocal record before the child starts,
        // so a restart always knows which child a call belongs to.
        let (record_tx, record_rx) = oneshot::channel();
        if parent
            .commands
            .send(Command::RecordDelegation {
                turn_id: request.parent_turn.clone(),
                tool_call_id: request.tool_call_id.clone(),
                child: child_name.clone(),
                agent: request.agent.clone(),
                reply: record_tx,
            })
            .await
            .is_err()
        {
            request
                .reply
                .send(fail(
                    &request.agent,
                    SubagentStatus::Failed,
                    "parent session stopped".to_owned(),
                ))
                .ok();
            return;
        }
        let record = tokio::time::timeout(std::time::Duration::from_secs(10), record_rx).await;
        let Ok(Ok(record)) = record else {
            request
                .reply
                .send(fail(
                    &request.agent,
                    SubagentStatus::UserCancelled,
                    "the parent turn is no longer active".to_owned(),
                ))
                .ok();
            return;
        };
        if let Err(reason) = record {
            request
                .reply
                .send(fail(&request.agent, SubagentStatus::UserCancelled, reason))
                .ok();
            return;
        }
        // Create the child on disk with its link and settings before its
        // actor starts, so it boots on the right agent and model. The
        // temporary session must be dropped before `self.load`: it still
        // holds the child's creation lock, and the load re-locks the same
        // file — flock conflicts even within one process.
        {
            let model = agent
                .model
                .clone()
                .unwrap_or_else(|| request.caller_model.clone());
            let Ok(mut child_session) =
                Session::new_in(self.inner.storage_root.clone(), Some(child_name.clone())).await
            else {
                request
                    .reply
                    .send(fail(
                        &request.agent,
                        SubagentStatus::Failed,
                        "could not create the child session".to_owned(),
                    ))
                    .ok();
                return;
            };
            child_session.meta.parent = Some(ParentRef {
                session: request.parent_session.clone(),
                turn: request.parent_turn.clone(),
                tool_call_id: request.tool_call_id.clone(),
                agent: request.agent.clone(),
                prompt: request.prompt.clone(),
            });
            child_session.meta.depth = parent.depth.saturating_add(1);
            child_session.meta.project_root = Some(self.inner.project_root.clone());
            child_session.meta.settings = Some(SessionSettings {
                model,
                reasoning_effort: None,
                agent: Some(request.agent.clone()),
            });
            if child_session.save().await.is_err() {
                request
                    .reply
                    .send(fail(
                        &request.agent,
                        SubagentStatus::Failed,
                        "could not save the child session".to_owned(),
                    ))
                    .ok();
                return;
            }
        }
        let loaded_child = match self.load(Some(child_name.clone()), false).await {
            Ok(_) => self.session(&child_name).await.ok(),
            Err(error) => {
                request
                    .reply
                    .send(fail(
                        &request.agent,
                        SubagentStatus::Failed,
                        format!("start child session: {error:#}"),
                    ))
                    .ok();
                return;
            }
        };
        let Some(loaded_child) = loaded_child else {
            request
                .reply
                .send(fail(
                    &request.agent,
                    SubagentStatus::Failed,
                    "child session stopped".to_owned(),
                ))
                .ok();
            return;
        };
        // Register the active delegation under the route lock so a
        // concurrent steer resolves against this record.
        {
            let _route = self.inner.route_lock.lock().await;
            let mut delegations = self.inner.delegations.lock().await;
            if let Some(existing) = delegations.get(&request.parent_session) {
                if existing.tool_call_id == request.tool_call_id {
                    request
                        .reply
                        .send(fail(
                            &request.agent,
                            SubagentStatus::Failed,
                            "duplicate delegation".to_owned(),
                        ))
                        .ok();
                    return;
                }
                request
                    .reply
                    .send(fail(
                        &request.agent,
                        SubagentStatus::Failed,
                        "the parent already has an active delegation".to_owned(),
                    ))
                    .ok();
                return;
            }
            delegations.insert(
                request.parent_session.clone(),
                ActiveDelegation {
                    parent_turn: request.parent_turn.clone(),
                    tool_call_id: request.tool_call_id.clone(),
                    child: child_name.clone(),
                    child_turn: None,
                },
            );
        }
        self.note_delegation(&request.parent_session, Some(&child_name))
            .await;
        // Submit the task. The child starts its delegated turn here.
        let accepted = self
            .send_command(
                &loaded_child,
                Vec::new(),
                Action::SendMessage {
                    content: request.prompt.clone(),
                    attachments: Vec::new(),
                },
            )
            .await;
        let Ok(accepted) = accepted else {
            self.finish_delegation(&request.parent_session, &child_name, false)
                .await;
            request
                .reply
                .send(fail(
                    &request.agent,
                    SubagentStatus::Failed,
                    "the child session stopped before starting".to_owned(),
                ))
                .ok();
            return;
        };
        {
            let mut delegations = self.inner.delegations.lock().await;
            if let Some(entry) = delegations.get_mut(&request.parent_session) {
                entry.child_turn = accepted.turn_id.clone();
            }
        }
        // Wait for the child to settle, or for the parent's call to stop
        // waiting (its turn was cancelled).
        let mut abandon = request.abandon.subscribe();
        let mut subscription = match self.subscribe(&child_name).await {
            Ok(subscription) => subscription,
            Err(_) => {
                self.finish_delegation(&request.parent_session, &child_name, false)
                    .await;
                request
                    .reply
                    .send(fail(
                        &request.agent,
                        SubagentStatus::Failed,
                        "child session stopped".to_owned(),
                    ))
                    .ok();
                return;
            }
        };
        let tokens_before = subscription.snapshot.state.total_tokens;
        let mut outcome: Option<SubagentOutcome> = None;
        'wait: loop {
            tokio::select! {
                abandoned = wait_abandoned(&mut abandon) => {
                    if abandoned {
                        // The parent stopped waiting: cancel the child's
                        // turn, then wait for it to actually settle.
                        let child_turn = {
                            let delegations = self.inner.delegations.lock().await;
                            delegations
                                .get(&request.parent_session)
                                .and_then(|entry| entry.child_turn.clone())
                        };
                        if let Some(turn) = child_turn {
                            let _ = self
                                .send_command(
                                    &loaded_child,
                                    Vec::new(),
                                    Action::Cancel { turn_id: turn },
                                )
                                .await;
                        }
                        self.wait_child_settled(&child_name).await;
                        outcome = Some(fail(
                            &request.agent,                            SubagentStatus::UserCancelled,
                            "user cancelled".to_owned(),
                        ));
                        break 'wait;
                    }
                }
                update = subscription.updates.recv() => {
                    let Ok(update) = update else { break 'wait; };
                    match &update.event {
                        Event::GenerationFinished { .. } => {
                            // The actor may start a follow-up turn just after
                            // publishing this finish event. Observe it after
                            // the actor has processed the queued steers.
                            let (seen, observed) = oneshot::channel();
                            if loaded_child.commands.send(Command::Observe(seen)).await.is_err()
                                || observed.await.is_err()
                            {
                                break 'wait;
                            }
                            let snapshot = self.subscribe(&child_name).await.ok().map(|s| s.snapshot);
                            if snapshot.as_ref().is_some_and(|s| {
                                s.state.turn_id.is_some() || s.state.queued_steers > 0
                            }) {
                                continue 'wait;
                            }
                            let response = snapshot.as_ref().and_then(|s| {
                                s.blocks
                                    .iter()
                                    .rev()
                                    .find(|block| {
                                        block.kind == crate::core::state::BlockKind::Assistant
                                            && !block.content.is_empty()
                                    })
                                    .map(|block| block.content.clone())
                            });
                            let tokens = snapshot
                                .as_ref()
                                .map(|s| s.state.total_tokens.saturating_sub(tokens_before));
                            outcome = Some(SubagentOutcome {
                                session_id: Some(child_name.clone()),
                                agent: request.agent.clone(),
                                status: SubagentStatus::Completed,
                                response,
                                error: None,
                                message: None,
                                tokens,
                            });
                            break 'wait;
                        }
                        Event::GenerationCancelled => {
                            outcome = Some(fail(
                                &request.agent,                                SubagentStatus::UserCancelled,
                                "user cancelled".to_owned(),
                            ));
                            break 'wait;
                        }
                        Event::Error(error) => {
                            outcome = Some(SubagentOutcome {
                                session_id: Some(child_name.clone()),
                                agent: request.agent.clone(),
                                status: SubagentStatus::Failed,
                                response: None,
                                error: Some(error.clone()),
                                message: None,
                                tokens: None,
                            });
                            break 'wait;
                        }
                        _ => {}
                    }
                }
            }
        }
        let outcome = outcome.unwrap_or_else(|| {
            fail(
                &request.agent,
                SubagentStatus::Failed,
                "the child session stopped".to_owned(),
            )
        });
        let settled = matches!(outcome.status, SubagentStatus::Completed);
        self.finish_delegation(&request.parent_session, &child_name, settled)
            .await;
        request.reply.send(outcome).ok();
    }

    /// Removes the parent's active-delegation record and publishes the
    /// updated child links to the parent's projection.
    async fn finish_delegation(&self, parent: &str, child: &str, settled: bool) {
        let _route = self.inner.route_lock.lock().await;
        {
            let mut delegations = self.inner.delegations.lock().await;
            if let Some(entry) = delegations.get(parent)
                && entry.child == child
            {
                delegations.remove(parent);
            }
        }
        self.note_delegation(parent, None).await;
        let _ = settled;
    }

    /// Cancels the parent's active delegation (if any) and waits for the
    /// child to settle before acknowledging.
    async fn cancel_delegation(&self, parent: &str, turn: &str) {
        let entry = {
            let _route = self.inner.route_lock.lock().await;
            let delegations = self.inner.delegations.lock().await;
            match delegations.get(parent) {
                Some(entry) if entry.parent_turn == turn => Some(entry.clone()),
                _ => None,
            }
        };
        let Some(entry) = entry else {
            return;
        };
        if let Some(child_turn) = entry.child_turn {
            if let Ok(session) = self.session(&entry.child).await {
                let _ = self
                    .send_command(
                        &session,
                        Vec::new(),
                        Action::Cancel {
                            turn_id: child_turn,
                        },
                    )
                    .await;
            }
        }
        self.wait_child_settled(&entry.child).await;
    }

    /// Waits (bounded) for a child session's active turn to end.
    async fn wait_child_settled(&self, child: &str) {
        let Ok(subscription) = self.subscribe(child).await else {
            return;
        };
        if subscription.snapshot.state.turn_id.is_none() {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut updates = subscription.updates;
        while let Ok(Ok(update)) =
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), updates.recv()).await
        {
            match &update.event {
                Event::GenerationFinished { .. } | Event::GenerationCancelled | Event::Error(_) => {
                    return;
                }
                _ => {}
            }
        }
    }

    /// Applies a delegation-state change to the parent's projection and
    /// publishes it to the parent's subscribers.
    async fn note_delegation(&self, parent: &str, child: Option<&str>) {
        let Ok(loaded) = self.session(parent).await else {
            return;
        };
        let mut links = self.child_links(&loaded, child);
        let mut hub = loaded.hub.lock().unwrap();
        for link in &mut links {
            if link.status == "unknown"
                && let Some(previous) = hub
                    .projection
                    .snapshot
                    .state
                    .children
                    .iter()
                    .find(|previous| previous.session == link.session)
            {
                link.status.clone_from(&previous.status);
            }
        }
        let event = Event::DelegationChanged {
            child: child.map(str::to_owned),
            children: links,
        };
        let changes = hub.projection.apply(&event);
        let seq = hub.projection.snapshot.seq;
        hub.updates
            .send(Arc::new(Update {
                session_id: parent.to_owned(),
                seq,
                changes,
                event,
            }))
            .ok();
    }

    /// The child links for one session, restored from its persisted records
    /// and transcript. The active child (if any) reports `running`.
    fn child_links(&self, loaded: &LoadedSession, active: Option<&str>) -> Vec<ChildLink> {
        let Ok(bytes) = std::fs::read(loaded.directory.join("session.json")) else {
            return Vec::new();
        };
        let Ok(meta) = serde_json::from_slice::<SessionMeta>(&bytes) else {
            return Vec::new();
        };
        let data =
            std::fs::read_to_string(loaded.directory.join("messages.jsonl")).unwrap_or_default();
        let mut statuses: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for line in data.lines() {
            let Ok(message) = serde_json::from_str::<crate::runtime::Message>(line) else {
                continue;
            };
            let crate::runtime::Message::Tool {
                call_id, content, ..
            } = &message
            else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
                continue;
            };
            if value.get("agent").is_none() || value.get("status").is_none() {
                continue;
            }
            if let Some(status) = value.get("status").and_then(serde_json::Value::as_str) {
                statuses.insert(call_id.clone(), status.to_owned());
            }
        }
        let mut links = Vec::new();
        for (tool_call_id, child) in &meta.delegations {
            if !meta.children.iter().any(|name| name == child) {
                continue;
            }
            let (child_agent, prompt) = {
                let path = self.inner.storage_root.join(child).join("session.json");
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                let Ok(child_meta) = serde_json::from_slice::<SessionMeta>(&bytes) else {
                    continue;
                };
                child_meta
                    .parent
                    .as_ref()
                    .map(|parent| (parent.agent.clone(), parent.prompt.clone()))
                    .unwrap_or_default()
            };
            let status = if Some(child.as_str()) == active {
                "running".to_owned()
            } else {
                statuses
                    .get(tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_owned())
            };
            links.push(ChildLink {
                session: child.clone(),
                agent: child_agent,
                prompt,
                tool_call_id: tool_call_id.clone(),
                status,
            });
        }
        links.sort_by(|a, b| a.session.cmp(&b.session));
        links
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

    /// Permanently removes a session and every child session it created:
    /// transcripts, attachments, and metadata. A loaded session in the
    /// subtree must be idle; an unloaded one is refused while its writer
    /// lock is held by another process.
    pub async fn delete(&self, id: &str) -> protocol::Result<()> {
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(protocol::Error::new("closed", "core is shutting down"));
        }
        let id = session::clean_name(id).map_err(core_error)?;
        let directory = self.inner.storage_root.join(&id);
        let unknown = || protocol::Error::new("unknown", format!("unknown session: {id}"));
        let bytes = tokio::fs::read(directory.join("session.json"))
            .await
            .map_err(|_| unknown())?;
        let meta: session::SessionMeta = serde_json::from_slice(&bytes).map_err(|_| unknown())?;
        if meta
            .project_root
            .as_ref()
            .is_some_and(|root| root != &self.inner.project_root)
        {
            return Err(protocol::Error::new(
                "unknown",
                format!("unknown session: {id}"),
            ));
        }
        // The children travel with the session: collect the whole subtree
        // so a deletion never leaves child sessions reachable from
        // nowhere. Stale records without a directory are skipped.
        let mut subtree = vec![id.clone()];
        let mut frontier = vec![meta.children.clone()];
        while let Some(children) = frontier.pop() {
            for child in children {
                if subtree.iter().any(|name| name == &child) {
                    continue;
                }
                let Ok(bytes) =
                    tokio::fs::read(self.inner.storage_root.join(&child).join("session.json"))
                        .await
                else {
                    continue;
                };
                let Ok(meta) = serde_json::from_slice::<session::SessionMeta>(&bytes) else {
                    continue;
                };
                if meta
                    .project_root
                    .as_ref()
                    .is_some_and(|root| root != &self.inner.project_root)
                {
                    continue;
                }
                subtree.push(child.clone());
                frontier.push(meta.children.clone());
            }
        }
        // Active delegation work blocks the deletion of either end of a
        // link: a live call still waits on its child, and a child still
        // answers its parent's call.
        {
            let delegations = self.inner.delegations.lock().await;
            if delegations.iter().any(|(parent, entry)| {
                subtree.iter().any(|name| name == parent)
                    || subtree.iter().any(|name| name == &entry.child)
            }) {
                return Err(protocol::Error::new(
                    "busy",
                    "finish or cancel the active delegation before deleting the session",
                ));
            }
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
        // A loaded session in the subtree must be idle; an unloaded one is
        // locked so a concurrent writer cannot resurrect it between the
        // check and the removal. The locks stay held across the removals.
        for name in &subtree {
            if let Some(loaded) = self.inner.sessions.lock().await.get(name) {
                let state = {
                    let hub = loaded.hub.lock().unwrap();
                    hub.projection.snapshot.state.clone()
                };
                if state.turn_id.is_some() || state.approval.is_some() {
                    return Err(protocol::Error::new(
                        "busy",
                        "finish or cancel the active operation before deleting the session",
                    ));
                }
            }
        }
        let mut locks = Vec::new();
        for name in &subtree {
            let directory = self.inner.storage_root.join(name);
            let loaded = self.inner.sessions.lock().await.remove(name);
            if loaded.is_none() {
                locks.push(session::lock_session(&directory).map_err(|error| {
                    protocol::Error::new(
                        "busy",
                        format!("session is owned by another process: {error:#}"),
                    )
                })?);
            }
            if let Some(loaded) = loaded {
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
        }
        for name in &subtree {
            if let Err(error) = tokio::fs::remove_dir_all(self.inner.storage_root.join(name)).await
            {
                return Err(protocol::Error::new(
                    "core",
                    format!("remove session directory: {error:#}"),
                ));
            }
        }
        drop(locks);
        let mut catalog = self.inner.catalog.lock().unwrap();
        let before = catalog.snapshot.sessions.len();
        catalog
            .snapshot
            .sessions
            .retain(|entry| !subtree.iter().any(|name| entry.info.name == *name));
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
        Ok(serde_json::to_value(
            session::store_file_upload(&session.directory, name, bytes.to_vec()).await?,
        )?)
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
        // Stop in-flight delegation work deepest-first while every
        // session is still responsive, so each session's own shutdown
        // cancellation finds nothing left to wait on.
        let mut chains = Vec::new();
        {
            let delegations = self.inner.delegations.lock().await;
            for (parent, entry) in delegations.iter() {
                chains.push((
                    parent.clone(),
                    entry.parent_turn.clone(),
                    entry.child.clone(),
                ));
            }
        }
        let children: std::collections::HashSet<&str> =
            chains.iter().map(|(_, _, child)| child.as_str()).collect();
        for (parent, turn, _) in chains
            .iter()
            .filter(|(_, _, child)| !children.contains(child.as_str()))
        {
            // Best effort during shutdown: a session that is already
            // unresponsive leaves its child to be dropped with it.
            let _ = self.cancel_descendants(parent).await;
            self.cancel_delegation(parent, turn).await;
        }
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
        // End the delegation loop: the session runtimes are gone, so
        // dropping the core's sender closes the port, then drain the
        // loop itself.
        *self.inner.delegation_tx.lock().unwrap() = None;
        if let Some(task) = self.inner.delegation_task.lock().unwrap().take() {
            task.await.ok();
        }
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

/// Drives the delegation port. Every command runs in its own task, so a
/// nested cancellation (a child cancelling its own delegation while its
/// parent's cancel is waiting for the child to settle) is never queued
/// behind the outer wait.
async fn delegation_loop(core: Core, mut rx: mpsc::UnboundedReceiver<DelegationCommand>) {
    while let Some(command) = rx.recv().await {
        match command {
            DelegationCommand::Spawn(request) => {
                let core = core.clone();
                tokio::spawn(async move {
                    core.handle_delegation(request).await;
                });
            }
            DelegationCommand::Cancel {
                parent_session,
                parent_turn,
                ack,
            } => {
                let core = core.clone();
                tokio::spawn(async move {
                    core.cancel_delegation(&parent_session, &parent_turn).await;
                    ack.send(()).ok();
                });
            }
        }
    }
}

/// Waits until the delegation's `abandon` flag is set or its sender drops.
/// Returns `true` only when the flag was seen set; a dropped sender (the
/// parent process is gone) waits out the child on the update branch.
async fn wait_abandoned(flag: &mut watch::Receiver<bool>) -> bool {
    loop {
        if *flag.borrow() {
            return true;
        }
        if flag.changed().await.is_err() {
            return *flag.borrow();
        }
    }
}

fn core_error(error: anyhow::Error) -> protocol::Error {
    protocol::Error::new("core", format!("{error:#}"))
}
