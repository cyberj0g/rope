#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use rope::{
    config::Config,
    core::{Core, Subscription, Update},
    protocol::Action,
    provider::{Provider, ResponseDelta, ResponseStream},
    runtime::{CompletionRequest, Event},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

pub struct ControlledProvider(pub mpsc::UnboundedSender<ModelRequest>);

pub struct ModelRequest {
    pub request: CompletionRequest,
    pub stream: mpsc::UnboundedSender<Result<ResponseDelta>>,
}

impl ModelRequest {
    pub fn finish(self, text: &str) {
        self.stream
            .send(Ok(ResponseDelta::Text(text.into())))
            .unwrap();
        self.stream.send(Ok(ResponseDelta::Completed)).unwrap();
    }

    pub fn tool(self, name: &str, arguments: serde_json::Value) {
        self.stream
            .send(Ok(ResponseDelta::ToolCall {
                index: 0,
                id: Some("call-1".into()),
                name: Some(name.into()),
                arguments: arguments.to_string(),
            }))
            .unwrap();
        self.stream.send(Ok(ResponseDelta::Completed)).unwrap();
    }
}

#[async_trait]
impl Provider for ControlledProvider {
    async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream> {
        let (stream, receiver) = mpsc::unbounded_channel();
        self.0.send(ModelRequest { request, stream })?;
        Ok(Box::pin(futures_util::stream::unfold(
            receiver,
            |mut receiver| async { receiver.recv().await.map(|item| (item, receiver)) },
        )))
    }
}

pub struct Harness {
    pub core: Core,
    pub project: tempfile::TempDir,
    pub storage: tempfile::TempDir,
    requests: mpsc::UnboundedReceiver<ModelRequest>,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with_config(Config::default()).await
    }

    pub async fn with_config(config: Config) -> Self {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (sender, requests) = mpsc::unbounded_channel();
        let core = Core::new(
            config,
            project.path().into(),
            storage.path().into(),
            Arc::new(ControlledProvider(sender)),
        )
        .await
        .unwrap();
        Self {
            core,
            project,
            storage,
            requests,
        }
    }

    pub async fn next(&mut self) -> ModelRequest {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

pub fn prompt(content: &str) -> Action {
    Action::SendMessage {
        content: content.into(),
        attachments: Vec::new(),
    }
}

pub async fn until(
    subscription: &mut Subscription,
    predicate: impl Fn(&Event) -> bool,
) -> Arc<Update> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = subscription.updates.recv().await.unwrap();
            if predicate(&event.event) {
                return event;
            }
        }
    })
    .await
    .expect("expected session event")
}
