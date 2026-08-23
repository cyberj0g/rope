use std::{collections::VecDeque, sync::Mutex};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream;

use super::{Provider, ResponseDelta, ResponseStream};
use crate::runtime::CompletionRequest;

pub struct MockProvider {
    responses: Mutex<VecDeque<Vec<ResponseDelta>>>,
}

impl MockProvider {
    pub fn new(responses: Vec<Vec<ResponseDelta>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(&self, _request: CompletionRequest) -> Result<ResponseStream> {
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("mock response exhausted");
        Ok(Box::pin(stream::iter(response.into_iter().map(Ok))))
    }
}
