use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use super::{Provider, ResponseDelta, ResponseStream};
use crate::runtime::{CompletionRequest, Message};

#[derive(Clone)]
pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
            api_key,
        }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut builder = self.client.post(url).json(&WireRequest::from(request));
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }
        let response = builder.send().await.context("send completion request")?;
        let status = response.status();
        if !status.is_success() {
            return response_error(status, response).await;
        }

        let events = response
            .bytes_stream()
            .eventsource()
            .filter_map(|event| async move {
                match event {
                    Err(error) => Some(Err(error.into())),
                    Ok(event) if event.data == "[DONE]" => None,
                    Ok(event) => Some(parse_delta(&event.data)),
                }
            })
            .flat_map(|result| match result {
                Ok(deltas) => futures_util::stream::iter(
                    deltas
                        .into_iter()
                        .map(Ok)
                        .collect::<Vec<Result<ResponseDelta>>>(),
                ),
                Err(error) => futures_util::stream::iter(vec![Err(error)]),
            });
        Ok(Box::pin(events))
    }
}

#[derive(Serialize)]
struct WireRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<crate::runtime::ReasoningEffort>,
    stream: bool,
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<crate::tool::ToolDefinition>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    r#type: &'static str,
    function: WireFunction,
}

#[derive(Serialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

impl From<CompletionRequest> for WireRequest {
    fn from(request: CompletionRequest) -> Self {
        Self {
            model: request.model,
            messages: request
                .messages
                .into_iter()
                .map(WireMessage::from)
                .collect(),
            temperature: request.temperature,
            reasoning_effort: request.reasoning_effort,
            stream: request.stream,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools: request.tools,
        }
    }
}

impl From<Message> for WireMessage {
    fn from(message: Message) -> Self {
        match message {
            Message::System { content } => Self::plain("system", content),
            Message::User { content } => Self::plain("user", content),
            Message::Assistant {
                content,
                reasoning: _,
                tool_calls,
            } => Self {
                role: "assistant",
                content: (!content.is_empty()).then_some(content),
                tool_calls: tool_calls
                    .into_iter()
                    .map(|call| WireToolCall {
                        id: call.id,
                        r#type: "function",
                        function: WireFunction {
                            name: call.name,
                            arguments: call.arguments.to_string(),
                        },
                    })
                    .collect(),
                tool_call_id: None,
            },
            Message::Tool { call_id, content } => Self {
                role: "tool",
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id),
            },
        }
    }
}

impl WireMessage {
    fn plain(role: &'static str, content: String) -> Self {
        Self {
            role,
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

async fn response_error(status: StatusCode, response: reqwest::Response) -> Result<ResponseStream> {
    let body = response.text().await.unwrap_or_default();
    bail!("server returned {status}: {body}")
}

fn parse_delta(data: &str) -> Result<Vec<ResponseDelta>> {
    #[derive(Deserialize)]
    struct Chunk {
        #[serde(default)]
        choices: Vec<Choice>,
        usage: Option<Usage>,
    }
    #[derive(Deserialize)]
    struct Choice {
        delta: Delta,
    }
    #[derive(Deserialize)]
    struct Delta {
        content: Option<String>,
        reasoning: Option<String>,
        reasoning_content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolDelta>,
    }
    #[derive(Deserialize)]
    struct ToolDelta {
        index: usize,
        id: Option<String>,
        function: Option<FunctionDelta>,
    }
    #[derive(Deserialize)]
    struct FunctionDelta {
        name: Option<String>,
        #[serde(default)]
        arguments: String,
    }
    #[derive(Deserialize)]
    struct Usage {
        total_tokens: u64,
    }

    let chunk: Chunk = serde_json::from_str(data).context("decode response chunk")?;
    let mut output = Vec::new();
    if let Some(delta) = chunk.choices.into_iter().next().map(|choice| choice.delta) {
        if let Some(reasoning) = delta
            .reasoning
            .or(delta.reasoning_content)
            .filter(|reasoning| !reasoning.is_empty())
        {
            output.push(ResponseDelta::Reasoning(reasoning));
        }
        if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
            output.push(ResponseDelta::Text(text));
        }
        output.extend(delta.tool_calls.into_iter().map(|call| {
            let function = call.function.unwrap_or(FunctionDelta {
                name: None,
                arguments: String::new(),
            });
            ResponseDelta::ToolCall {
                index: call.index,
                id: call.id,
                name: function.name,
                arguments: function.arguments,
            }
        }));
    }
    if let Some(usage) = chunk.usage {
        output.push(ResponseDelta::Usage(usage.total_tokens));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_and_tool_deltas() {
        let text = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
        assert_eq!(
            parse_delta(text).unwrap(),
            vec![ResponseDelta::Text("hello".into())]
        );
        let tool = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{}"}}]}}]}"#;
        assert!(matches!(
            parse_delta(tool).unwrap()[0],
            ResponseDelta::ToolCall { index: 0, .. }
        ));

        let reasoning = r#"{"choices":[{"delta":{"reasoning":"think"}}]}"#;
        assert_eq!(
            parse_delta(reasoning).unwrap(),
            vec![ResponseDelta::Reasoning("think".into())]
        );

        let usage = r#"{"choices":[],"usage":{"total_tokens":321}}"#;
        assert_eq!(parse_delta(usage).unwrap(), vec![ResponseDelta::Usage(321)]);
        let legacy = r#"{"choices":[{"delta":{"reasoning_content":"legacy"}}]}"#;
        assert_eq!(
            parse_delta(legacy).unwrap(),
            vec![ResponseDelta::Reasoning("legacy".into())]
        );
    }
}
