#[cfg(test)]
pub mod mock;
pub mod openai;

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::Stream;

use crate::runtime::CompletionRequest;

pub type ResponseStream = Pin<Box<dyn Stream<Item = Result<ResponseDelta>> + Send>>;

#[derive(Clone, Debug, PartialEq)]
pub enum ResponseDelta {
    Reasoning(String),
    Text(String),
    Usage(Usage),
    ToolCall {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream>;
}
