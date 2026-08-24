use std::{collections::VecDeque, sync::Mutex};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream;

use super::{Provider, ResponseDelta, ResponseStream};
use crate::runtime::CompletionRequest;

pub struct MockProvider {
    responses: Mutex<VecDeque<Vec<ResponseDelta>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl MockProvider {
    pub fn new(responses: Vec<Vec<ResponseDelta>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("mock response exhausted");
        Ok(Box::pin(stream::iter(response.into_iter().map(Ok))))
    }
}
