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
    Usage(u64),
    ToolCall {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream>;
}
