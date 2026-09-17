use super::*;
use crate::{
    protocol::{Accepted, Action, Error as CommandError},
    session::SessionSettings,
};

pub fn spawn_session<P: Provider + ?Sized>(
    config: Config,
    provider: Arc<P>,
    tools: ToolRegistry,
    session: Session,
    messages: Vec<Message>,
    project: ProjectState,
) -> (mpsc::Sender<Command>, mpsc::Receiver<Event>, JoinHandle<()>) {
    let (commands, receiver) = mpsc::channel(16);
    let (events, output) = mpsc::channel(64);
    let task = tokio::spawn(run(
        config, provider, tools, session, messages, project, receiver, events,
    ));
    (commands, output, task)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run<P: Provider + ?Sized>(
    mut config: Config,
    provider: Arc<P>,
    tools: ToolRegistry,
    mut session: Session,
    mut messages: Vec<Message>,
    project: ProjectState,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
) {
    let (internal_tx, mut internal_rx) = mpsc::channel(64);
    let mut generation: Option<ActiveTurn> = None;
    let mut compacting: Option<(String, JoinHandle<()>)> = None;
    let mut pending_approval: Option<PendingApproval> = None;
    let mut pending_prompts: SteerQueue = Arc::new(Mutex::new(Vec::new()));

    let mut settings_revision = 0;

    if let Some(settings) = &session.meta.settings {
        if config.select_model(&settings.model).is_ok() {
            config.reasoning_effort = settings.reasoning_effort;
        } else {
            events
                .send(Event::Notice(format!(
                    "saved model '{}' is unavailable; using '{}'",
                    settings.model,
                    config.model_name()
                )))
                .await
                .ok();
        }
    }
    events.send(Event::History(messages.clone())).await.ok();
    events
        .send(Event::SessionChanged(session.display_name().to_owned()))
        .await
        .ok();
    send_usage(&events, &session).await;
    send_settings(&events, &config).await;
    send_context(&events, &session, &config).await;
    events
        .send(Event::ProjectChanged(project.clone()))
        .await
        .ok();
    events
        .send(Event::PlanChanged(session.meta.plan.clone()))
        .await
        .ok();
    events.send(Event::Ready).await.ok();

    loop {
        tokio::select! {
            command = commands.recv() => {
                let command = match command {
                    Some(command) => command,
                    None => { let (reply, _) = oneshot::channel(); Command::Shutdown(reply) }
                };
                let mut response = None;
                let mut publication = None;
                let command = if let Command::Request { action, images, reply, published } = command {
                    let operation = generation.as_ref().map(|turn| turn.id.as_str())
                        .or_else(|| compacting.as_ref().map(|(id, _)| id.as_str()));
                    match validate(action, images, operation, compacting.is_some(), pending_approval.as_ref(), settings_revision, &config) {
                        Ok(command) => { response = Some(reply); publication = Some(published); command }
                        Err(error) => { reply.send(Err(error)).ok(); continue; }
                    }
                } else { command };
                let mut error = None;
                match command {
                    Command::Submit(prompt) | Command::Steer(prompt) => {
                        if compacting.is_some() {
                            error = Some(CommandError::new("busy", "manual compaction is running"));
                        } else if generation.is_some() {
                            events.send(Event::MessageAccepted(prompt.clone().steer_message())).await.ok();
                            pending_prompts.lock().unwrap().push(prompt);
                        } else {
                            let first = Message::user_with_images(prompt.content, prompt.images);
                            events.send(Event::MessageAccepted(first.clone())).await.ok();
                            spawn_turn(&mut generation, &mut pending_prompts, &mut messages,
                                &mut session, &project, &provider, &tools, &config, first,
                                &events, &internal_tx).await;
                        }
                    }
                    Command::Cancel => {
                        if let Some(active) = generation.take() {
                            active.task.abort();
                            active.task.await.ok();
                            tools.cancel_active().await;
                            pending_approval = None;
                            if let Err(failure) = persist_interrupted_turn(&mut messages, &mut session,
                                active.progress, &pending_prompts, CANCELLED_BY_USER).await {
                                error = Some(CommandError::new("persistence", format!("save cancelled turn: {failure:#}")));
                            }
                        }
                        if let Some((_, task)) = compacting.take() {
                            task.abort(); task.await.ok();
                        }
                        events.send(Event::GenerationCancelled).await.ok();
                        events.send(Event::RefreshProject).await.ok();
                    }
                    Command::Approve(decision) => {
                        if let Some(pending) = pending_approval.take() {
                            if decision == ApprovalDecision::AllowSession
                                && !session.meta.approved_tools.contains(&pending.tool) {
                                session.meta.approved_tools.push(pending.tool.clone());
                                if let Err(failure) = session.save().await {
                                    session.meta.approved_tools.retain(|tool| tool != &pending.tool);
                                    error = Some(CommandError::new("persistence", format!("save approval: {failure:#}")));
                                }
                            }
                            let decision = if error.is_some() { ApprovalDecision::Deny } else { decision };
                            events.send(Event::ApprovalResolved { approval_id: pending.id, tool: pending.tool, decision }).await.ok();
                            pending.reply.send(decision).ok();
                        }
                    }
                    Command::SelectModel(_) | Command::SetReasoning(_)
                        if generation.is_some() || compacting.is_some() => {
                            error = Some(CommandError::new("busy", "finish or cancel the active operation before changing settings"));
                        }
                    Command::SelectModel(_) | Command::SetReasoning(_) => {
                        let old = config.clone();
                        let changed = match command {
                            Command::SelectModel(model) => config.select_model(&model).map(|()| {
                                config.reasoning_effort = config.active_model().reasoning_effort;
                            }),
                            Command::SetReasoning(effort) => { config.reasoning_effort = effort; Ok(()) }
                            _ => unreachable!(),
                        };
                        if let Err(failure) = changed {
                            error = Some(CommandError::new("invalid_settings", failure.to_string()));
                        } else {
                            let old_settings = session.meta.settings.clone();
                            session.meta.settings = Some(SessionSettings { model: config.model_name().into(), reasoning_effort: config.effective_reasoning_effort() });
                            if let Err(failure) = session.save().await {
                                config = old; session.meta.settings = old_settings;
                                error = Some(CommandError::new("persistence", format!("save settings: {failure:#}")));
                            } else {
                                settings_revision += 1;
                                events.send(Event::SettingsRevision(settings_revision)).await.ok();
                                send_settings(&events, &config).await;
                                send_context(&events, &session, &config).await;
                            }
                        }
                    }
                    Command::Compact if generation.is_none() && compacting.is_none() => {
                        let context = request_context(&messages, &session.meta);
                        if context.len() <= 1 {
                            error = Some(CommandError::new("empty", "nothing to compact yet"));
                        } else {
                            let id = uuid::Uuid::new_v4().to_string();
                            events.send(Event::OperationStarted { id: id.clone(), compacting: true }).await.ok();
                            let provider = provider.clone(); let config = config.clone();
                            let parent = internal_tx.clone(); let operation_id = id.clone();
                            let (worker_events, worker_internal, forward) = worker_channels(&id, &internal_tx);
                            compacting = Some((id, tokio::spawn(async move {
                                let result = summarize(provider, &config, &context, &worker_events, &worker_internal).await
                                    .map_err(|error| format!("compact: {error:#}"));
                                drop(worker_events); drop(worker_internal); forward.await.ok();
                                parent.send(InternalEvent::Scoped { id: operation_id, event: Box::new(InternalEvent::Compacted(result)) }).await.ok();
                            })));
                        }
                    }
                    Command::Compact => error = Some(CommandError::new("busy", "an operation is running")),
                    Command::Shutdown(reply) => {
                        if let Some(active) = generation.take() {
                            active.task.abort(); active.task.await.ok();
                            if let Err(failure) = persist_interrupted_turn(&mut messages, &mut session, active.progress, &pending_prompts, CANCELLED_BY_USER).await {
                                error = Some(CommandError::new("persistence", format!("save interrupted turn: {failure:#}")));
                            }
                        }
                        if let Some((_, task)) = compacting.take() { task.abort(); task.await.ok(); }
                        tools.shutdown().await;
                        if let Err(failure) = session.save().await {
                            error = Some(CommandError::new("persistence", format!("save session: {failure:#}")));
                        }
                        reply.send(SessionSummary { name: session.meta.name.clone(), total_tokens: session.meta.total_tokens,
                            total_cost: session.total_cost(), error: error.map(|e| e.to_string()) }).ok();
                        break;
                    }
                    Command::Request { .. } => unreachable!(),
                }
                if let Some(reply) = response {
                    if let Some(published) = publication {
                        let published = Arc::new(Mutex::new(Some(published)));
                        events.send(Event::Barrier(published)).await.ok();
                    }
                    reply.send(match error {
                        Some(error) => Err(error),
                        None => Ok(Accepted { turn_id: generation.as_ref().map(|turn| turn.id.clone())
                            .or_else(|| compacting.as_ref().map(|(id, _)| id.clone())), settings_revision }),
                    }).ok();
                } else if let Some(error) = error {
                    events.send(Event::Error(error.message)).await.ok();
                }
            }
            Some(event) = internal_rx.recv() => {
                let event = if let InternalEvent::Scoped { id, event } = event {
                    if generation.as_ref().is_none_or(|turn| turn.id != id)
                        && compacting.as_ref().is_none_or(|(active, _)| *active != id) { continue; }
                    *event
                } else { event };
                match event {
                    InternalEvent::Visible(Event::ContextCompacted { .. }) if compacting.is_some() => {}
                    InternalEvent::Visible(event) => { events.send(event).await.ok(); }
                    InternalEvent::Compacted(result) => {
                        compacting = None;
                        let result = match result {
                            Ok(summary) => {
                                let marker = Message::system(format!("{COMPACTION_MARKER}\n{summary}"));
                                let saved = async {
                                    session.append(std::slice::from_ref(&marker)).await?;
                                    session.meta.compaction_summary = Some(summary.clone());
                                    session.meta.compacted_through = messages.len();
                                    messages.push(marker);
                                    session.meta.context_tokens = estimate_tokens(&request_context(&messages, &session.meta));
                                    session.save().await
                                }.await;
                                if saved.is_ok() { events.send(Event::ContextCompacted { summary }).await.ok(); }
                                saved.map_err(|e| format!("save compaction: {e:#}"))
                            }
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(()) => { send_context(&events, &session, &config).await; events.send(Event::GenerationFinished).await.ok(); }
                            Err(error) => { events.send(Event::Error(error)).await.ok(); }
                        }
                    }
                    InternalEvent::Finished(result) if generation.is_some() => {
                        generation = None; pending_approval = None;
                        tools.cancel_active().await;
                        messages.truncate(messages.len().saturating_sub(1));
                        let TurnResult { completed, compaction, title } = result;
                        if let Some(title) = title {
                            session.set_title(title);
                            events.send(Event::SessionChanged(session.display_name().to_owned())).await.ok();
                        }
                        let mut persisted = Vec::new();
                        if let Some(compaction) = compaction {
                            let marker = Message::system(format!("{COMPACTION_MARKER}\n{}", compaction.summary));
                            session.meta.compaction_summary = Some(compaction.summary);
                            session.meta.compacted_through = compaction.through;
                            messages.push(marker.clone()); persisted.push(marker);
                        }
                        messages.extend(completed.clone()); persisted.extend(completed);
                        let projected = request_context(&messages, &session.meta);
                        if projected.iter().any(is_ejected_web_result) {
                            session.meta.context_tokens = estimate_tokens(&projected);
                            send_context(&events, &session, &config).await;
                        }
                        let saved = async { session.append(&persisted).await?; session.save().await }.await;
                        if let Err(error) = saved { events.send(Event::Error(format!("save session: {error:#}"))).await.ok(); }
                        else { events.send(Event::GenerationFinished).await.ok(); }
                        events.send(Event::RefreshProject).await.ok();
                        let mut steered = pending_prompts.lock().unwrap().drain(..).collect::<Vec<_>>();
                        if !steered.is_empty() {
                            let first = steered.remove(0);
                            events.send(Event::SteersDelivered(1)).await.ok();
                            spawn_turn(&mut generation, &mut pending_prompts, &mut messages, &mut session,
                                &project, &provider, &tools, &config, first.steer_message(), &events, &internal_tx).await;
                            pending_prompts.lock().unwrap().extend(steered);
                        }
                    }
                    InternalEvent::Failed(error) if generation.is_some() => {
                        let active = generation.take().unwrap(); pending_approval = None;
                        tools.cancel_active().await;
                        if let Err(failure) = persist_interrupted_turn(&mut messages, &mut session, active.progress, &pending_prompts, &format!("turn failed: {error}")).await {
                            events.send(Event::Notice(format!("save failed turn: {failure:#}"))).await.ok();
                        }
                        events.send(Event::Error(error)).await.ok();
                        events.send(Event::RefreshProject).await.ok();
                    }
                    InternalEvent::Usage(usage) => {
                        session.record_usage(usage.total_tokens, config.active_model().price_per_token);
                        if compacting.is_none() { session.meta.context_tokens = usage.total_tokens; }
                        send_usage(&events, &session).await;
                        send_context(&events, &session, &config).await;
                    }
                    InternalEvent::AuxiliaryUsage(usage) => {
                        session.record_usage(usage.total_tokens, config.active_model().price_per_token);
                        send_usage(&events, &session).await;
                    }
                    InternalEvent::PlanUpdated(plan) if generation.is_some() => {
                        session.meta.plan = Some(plan.clone());
                        if let Err(error) = session.save().await { events.send(Event::Notice(format!("save plan: {error:#}"))).await.ok(); }
                        events.send(Event::PlanChanged(Some(plan))).await.ok();
                    }
                    InternalEvent::Approval { call, reply } => {
                        if generation.is_none() || pending_approval.is_some() { reply.send(ApprovalDecision::Deny).ok(); }
                        else if session.meta.approved_tools.contains(&call.name) { reply.send(ApprovalDecision::AllowSession).ok(); }
                        else {
                            let id = uuid::Uuid::new_v4().to_string();
                            pending_approval = Some(PendingApproval { id: id.clone(), tool: call.name.clone(), reply });
                            events.send(Event::ApprovalRequested { approval_id: id, call }).await.ok();
                        }
                    }
                    InternalEvent::ProjectRefresh => { events.send(Event::RefreshProject).await.ok(); }
                    InternalEvent::Finished(_) | InternalEvent::Failed(_) | InternalEvent::PlanUpdated(_) => {}
                    InternalEvent::Scoped { .. } => unreachable!(),
                }
            }
        }
    }
}

fn validate(
    action: Action,
    images: Vec<ImageContent>,
    operation: Option<&str>,
    compacting: bool,
    approval: Option<&PendingApproval>,
    revision: u64,
    config: &Config,
) -> crate::protocol::Result<Command> {
    let idle = || {
        if operation.is_some() {
            Err(CommandError::new("busy", "an operation is running"))
        } else {
            Ok(())
        }
    };
    let same_turn = |id: &str| {
        if operation != Some(id) {
            Err(CommandError::new(
                "stale_turn",
                "the operation is no longer active",
            ))
        } else {
            Ok(())
        }
    };
    let same_settings = |expected| {
        if expected != revision {
            Err(CommandError::new(
                "stale_settings",
                "session settings changed; refresh and try again",
            ))
        } else {
            Ok(())
        }
    };
    Ok(match action {
        Action::SendMessage { content, .. } => {
            if compacting {
                return Err(CommandError::new("busy", "manual compaction is running"));
            }
            if content.trim().is_empty() && images.is_empty() {
                return Err(CommandError::new("empty", "message is empty"));
            }
            if !images.is_empty() && !config.active_model().vision {
                return Err(CommandError::new(
                    "unsupported",
                    "the selected model does not support images",
                ));
            }
            Command::Submit(UserPrompt { content, images })
        }
        Action::Cancel { turn_id } => {
            same_turn(&turn_id)?;
            Command::Cancel
        }
        Action::Approve {
            turn_id,
            approval_id,
            decision,
        } => {
            same_turn(&turn_id)?;
            if approval.is_none_or(|pending| pending.id != approval_id) {
                return Err(CommandError::new(
                    "stale_approval",
                    "the approval was already resolved or replaced",
                ));
            }
            Command::Approve(decision)
        }
        Action::SetModel {
            model,
            revision: expected,
        } => {
            idle()?;
            same_settings(expected)?;
            Command::SelectModel(model)
        }
        Action::SetReasoning {
            effort,
            revision: expected,
        } => {
            idle()?;
            same_settings(expected)?;
            if effort.is_some_and(|e| !config.active_model().reasoning_efforts.contains(&e)) {
                return Err(CommandError::new(
                    "invalid_settings",
                    "unsupported reasoning effort",
                ));
            }
            Command::SetReasoning(effort)
        }
        Action::Compact => {
            idle()?;
            Command::Compact
        }
    })
}
