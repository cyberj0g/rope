use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::Config,
    core::Core,
    protocol::Action,
    runtime::{Event, SessionSummary, UserPrompt},
};

pub enum Command {
    Submit(UserPrompt),
    Steer(UserPrompt),
    Action(Action),
    NextReasoning {
        model: String,
        current: Option<crate::runtime::ReasoningEffort>,
        revision: u64,
    },
    NewSession(Option<String>),
    ResumeSession(String),
    Remember(String),
    GitDiff(Option<std::path::PathBuf>),
    Shutdown(oneshot::Sender<SessionSummary>),
}

pub struct Connection(JoinHandle<()>);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn connect(
    core: Core,
    mut preferences: Config,
    mut selected: String,
) -> Result<(mpsc::Sender<Command>, mpsc::Receiver<Event>, Connection)> {
    let mut subscription = core.subscribe(&selected).await?;
    let (catalog, mut catalogs) = core.subscribe_catalog();
    let mut project = core.subscribe_project();
    let (commands, mut requests) = mpsc::channel(16);
    let (events, output) = mpsc::channel(64);
    events
        .send(Event::Catalog(
            catalog
                .sessions
                .into_iter()
                .map(|entry| entry.info)
                .collect(),
        ))
        .await?;
    events
        .send(Event::Snapshot(Box::new(subscription.snapshot.clone())))
        .await?;
    let task = tokio::spawn(async move {
        loop {
            let result: Result<()> = async {
                tokio::select! {
                    command = requests.recv() => {
                        let Some(command) = command else { return Ok(()); };
                        match command {
                            Command::Submit(prompt) | Command::Steer(prompt) => {
                                let submitted = async {
                                    let mut attachments = Vec::new();
                                    for image in &prompt.images {
                                        let stored = core.attach(&selected, &STANDARD.decode(&image.data)?).await?;
                                        attachments.push(stored.path.unwrap());
                                    }
                                    core.command(&selected, Action::SendMessage { content: prompt.content.clone(), attachments }).await?;
                                    Ok::<_, anyhow::Error>(())
                                }.await;
                                if let Err(error) = submitted { events.send(Event::PromptRejected(prompt, format!("{error:#}"))).await?; }
                            }
                            Command::Action(action) => {
                                core.command(&selected, action.clone()).await?;
                                if let Action::SetModel { model, .. } = action {
                                    preferences.remember_model_choice(&model)?;
                                }
                            }
                            Command::NextReasoning { model, current, revision } => {
                                let model = preferences.models.iter().find(|entry| entry.name == model || entry.id == model)
                                    .ok_or_else(|| anyhow::anyhow!("unknown model"))?;
                                let efforts = &model.reasoning_efforts;
                                let next = current.and_then(|effort| efforts.iter().position(|value| *value == effort))
                                    .map_or(0, |i| (i + 1) % efforts.len().max(1));
                                core.command(&selected, Action::SetReasoning { effort: efforts.get(next).copied(), revision }).await?;
                            }
                            Command::NewSession(name) => {
                                selected = core.create(name).await?;
                                subscription = core.subscribe(&selected).await?;
                                events.send(Event::Snapshot(Box::new(subscription.snapshot.clone()))).await?;
                            }
                            Command::ResumeSession(id) => {
                                let next = core.subscribe(&id).await?;
                                selected = id; subscription = next;
                                events.send(Event::Snapshot(Box::new(subscription.snapshot.clone()))).await?;
                            }
                            Command::Remember(command) => preferences.remember_command(&command)?,
                            Command::GitDiff(path) => {
                                let content = core.diff(path.as_deref()).await?;
                                events.send(Event::Diff { path, content }).await?;
                            }
                            Command::Shutdown(reply) => {
                                let snapshot = core.subscribe(&selected).await?.snapshot;
                                reply.send(SessionSummary { name: selected.clone(), total_tokens: snapshot.state.total_tokens,
                                    total_cost: snapshot.state.total_cost, error: None }).ok();
                                return Ok(());
                            }
                        }
                    }
                    update = subscription.updates.recv() => match update {
                        Ok(update) => events.send(Event::Update(update)).await?,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            subscription = core.subscribe(&selected).await?;
                            events.send(Event::Snapshot(Box::new(subscription.snapshot.clone()))).await?;
                        }
                        Err(broadcast::error::RecvError::Closed) => anyhow::bail!("session stopped"),
                    },
                    catalog = catalogs.recv() => {
                        let catalog = match catalog {
                            Ok(catalog) => catalog,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let (snapshot, receiver) = core.subscribe_catalog(); catalogs = receiver; snapshot
                            }
                            Err(broadcast::error::RecvError::Closed) => anyhow::bail!("core stopped"),
                        };
                        events.send(Event::Catalog(catalog.sessions.into_iter().map(|entry| entry.info).collect())).await?;
                    }
                    changed = project.changed() => {
                        changed?;
                        let snapshot = project.borrow_and_update().project.clone();
                        events.send(Event::ProjectChanged(snapshot)).await?;
                    }
                }
                Ok(())
            }.await;
            if events.is_closed() || requests.is_closed() {
                break;
            }
            if let Err(error) = result
                && events
                    .send(Event::Notice(format!("{error:#}")))
                    .await
                    .is_err()
            {
                break;
            }
        }
    });
    Ok((commands, output, Connection(task)))
}
