use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};

use super::{ResponseDelta, ResponseStream, Usage, openai::response_error};
use crate::{
    runtime::{CompletionRequest, ImageContent, Message, ReasoningEffort},
    tool::ToolDefinition,
};

pub async fn stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: CompletionRequest,
) -> Result<ResponseStream> {
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let mut builder = client.post(url).json(&ResponseRequest::from(request));
    if !api_key.is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    let response = builder.send().await.context("send Responses API request")?;
    let status = response.status();
    if !status.is_success() {
        return response_error(status, response).await;
    }

    let mut decoder = ResponsesDecoder::default();
    let events = response
        .bytes_stream()
        .eventsource()
        .filter_map(move |event| {
            let result = match event {
                Err(error) => Some(Err(error.into())),
                Ok(event) if event.data == "[DONE]" => None,
                Ok(event) => Some(decoder.parse(&event.data)),
            };
            async move { result }
        })
        .flat_map(|result| match result {
            Ok(deltas) => {
                futures_util::stream::iter(deltas.into_iter().map(Ok).collect::<Vec<_>>())
            }
            Err(error) => futures_util::stream::iter(vec![Err(error)]),
        });
    Ok(Box::pin(events))
}

#[derive(Serialize)]
struct ResponseRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponseReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    stream: bool,
    store: bool,
    include: [&'static str; 1],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponseTool>,
}

#[derive(Serialize)]
struct ResponseReasoning {
    effort: ReasoningEffort,
    context: &'static str,
    summary: &'static str,
}

#[derive(Serialize)]
struct ResponseTool {
    r#type: &'static str,
    name: String,
    description: String,
    parameters: Value,
}

impl From<CompletionRequest> for ResponseRequest {
    fn from(request: CompletionRequest) -> Self {
        let input = input_items(request.messages, &request.model);
        Self {
            model: request.model,
            input,
            temperature: request.temperature,
            reasoning: request.reasoning_effort.map(|effort| ResponseReasoning {
                effort,
                context: "all_turns",
                summary: "auto",
            }),
            max_output_tokens: request.max_tokens,
            stream: request.stream,
            store: false,
            include: ["reasoning.encrypted_content"],
            tools: request.tools.into_iter().map(ResponseTool::from).collect(),
        }
    }
}

impl From<ToolDefinition> for ResponseTool {
    fn from(tool: ToolDefinition) -> Self {
        Self {
            r#type: "function",
            name: tool.function.name,
            description: tool.function.description,
            parameters: tool.function.parameters,
        }
    }
}

fn input_items(messages: Vec<Message>, model: &str) -> Vec<Value> {
    messages
        .into_iter()
        .flat_map(|message| message_items(message, model))
        .collect()
}

fn message_items(message: Message, model: &str) -> Vec<Value> {
    match message {
        Message::System { content } => vec![json!({ "role": "system", "content": content })],
        Message::User { content, images } if images.is_empty() => {
            vec![json!({ "role": "user", "content": content })]
        }
        Message::User { content, images } => vec![json!({
            "role": "user",
            "content": response_content(content, images),
        })],
        Message::Assistant {
            content,
            model: source_model,
            tool_calls,
            response_items,
            ..
        } if response_items.is_empty() || source_model != model => {
            let mut items = Vec::with_capacity(tool_calls.len() + usize::from(!content.is_empty()));
            if !content.is_empty() {
                items.push(json!({ "role": "assistant", "content": content }));
            }
            items.extend(tool_calls.into_iter().map(|call| {
                json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments.to_string(),
                })
            }));
            items
        }
        Message::Assistant {
            content,
            tool_calls,
            mut response_items,
            ..
        } => {
            for item in &mut response_items {
                if let Some(item) = item.as_object_mut() {
                    item.remove("status");
                }
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    continue;
                }
                let Some(call) = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| tool_calls.iter().find(|call| call.id == id))
                else {
                    continue;
                };
                item["name"] = Value::String(call.name.clone());
                item["arguments"] = Value::String(call.arguments.to_string());
            }
            if !content.is_empty()
                && !response_items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("message"))
            {
                response_items.push(json!({ "role": "assistant", "content": content }));
            }
            for call in tool_calls {
                if response_items.iter().any(|item| {
                    item.get("call_id").and_then(Value::as_str) == Some(call.id.as_str())
                }) {
                    continue;
                }
                response_items.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments.to_string(),
                }));
            }
            response_items
        }
        Message::Tool {
            call_id,
            content,
            image,
            ..
        } => vec![json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": match image {
                Some(image) => Value::Array(response_content(content, vec![image])),
                None => Value::String(content),
            },
        })],
    }
}

fn response_content(content: String, images: Vec<ImageContent>) -> Vec<Value> {
    let mut parts = Vec::with_capacity(images.len() + usize::from(!content.is_empty()));
    if !content.is_empty() {
        parts.push(json!({ "type": "input_text", "text": content }));
    }
    parts.extend(images.into_iter().map(|image| {
        json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
        })
    }));
    parts
}

#[derive(Default)]
struct ResponsesDecoder {
    tool_indexes: HashMap<String, usize>,
    output_indexes: HashMap<usize, usize>,
    tool_arguments: HashMap<String, String>,
    announced_tools: HashSet<String>,
    next_tool: usize,
}

impl ResponsesDecoder {
    fn parse(&mut self, data: &str) -> Result<Vec<ResponseDelta>> {
        let event: Value = serde_json::from_str(data).context("decode Responses API event")?;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => Ok(delta(&event)
                .filter(|delta| !delta.is_empty())
                .map(|delta| vec![ResponseDelta::Text(delta.to_owned())])
                .unwrap_or_default()),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                Ok(delta(&event)
                    .filter(|delta| !delta.is_empty())
                    .map(|delta| vec![ResponseDelta::Reasoning(delta.to_owned())])
                    .unwrap_or_default())
            }
            "response.output_item.added" => Ok(self.tool_added(&event)),
            "response.function_call_arguments.delta" => Ok(self.tool_arguments_delta(&event)),
            "response.output_item.done" => Ok(self.item_done(&event)),
            "response.completed" | "response.incomplete" => Ok(usage(&event)
                .map(|usage| vec![ResponseDelta::Usage(usage)])
                .unwrap_or_default()),
            "response.failed" | "error" => {
                let error = event
                    .pointer("/response/error/message")
                    .or_else(|| event.pointer("/error/message"))
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Responses API request failed");
                bail!("{error}")
            }
            _ => Ok(Vec::new()),
        }
    }

    fn tool_added(&mut self, event: &Value) -> Vec<ResponseDelta> {
        let Some(item) = event.get("item") else {
            return Vec::new();
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Vec::new();
        }
        let key = item_key(item, event);
        let index = self.tool_index(&key, event);
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.tool_arguments.insert(key.clone(), arguments.clone());
        self.announced_tools.insert(key);
        vec![ResponseDelta::ToolCall {
            index,
            id: item
                .get("call_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            name: item.get("name").and_then(Value::as_str).map(str::to_owned),
            arguments,
        }]
    }

    fn tool_arguments_delta(&mut self, event: &Value) -> Vec<ResponseDelta> {
        let key = item_key(event, event);
        let index = self.tool_index(&key, event);
        let arguments = delta(event).unwrap_or_default().to_owned();
        self.tool_arguments
            .entry(key)
            .or_default()
            .push_str(&arguments);
        vec![ResponseDelta::ToolCall {
            index,
            id: None,
            name: None,
            arguments,
        }]
    }

    fn item_done(&mut self, event: &Value) -> Vec<ResponseDelta> {
        let Some(item) = event.get("item") else {
            return Vec::new();
        };
        let mut output = Vec::new();
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            let key = item_key(item, event);
            let index = self.tool_index(&key, event);
            let announced = self.announced_tools.insert(key.clone());
            let complete = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let streamed = self
                .tool_arguments
                .get(&key)
                .map(String::as_str)
                .unwrap_or_default();
            let missing = complete.strip_prefix(streamed).unwrap_or_default();
            if announced || !missing.is_empty() {
                output.push(ResponseDelta::ToolCall {
                    index,
                    id: announced
                        .then(|| {
                            item.get("call_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .flatten(),
                    name: announced
                        .then(|| item.get("name").and_then(Value::as_str).map(str::to_owned))
                        .flatten(),
                    arguments: missing.to_owned(),
                });
            }
        }
        output.push(ResponseDelta::OutputItem(item.clone()));
        output
    }

    fn tool_index(&mut self, key: &str, event: &Value) -> usize {
        if let Some(index) = self.tool_indexes.get(key) {
            return *index;
        }
        let output_index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        if let Some(index) = output_index.and_then(|index| self.output_indexes.get(&index).copied())
        {
            self.tool_indexes.insert(key.to_owned(), index);
            return index;
        }
        let index = self.next_tool;
        self.next_tool += 1;
        self.tool_indexes.insert(key.to_owned(), index);
        if let Some(output_index) = output_index {
            self.output_indexes.insert(output_index, index);
        }
        index
    }
}

fn item_key(item: &Value, event: &Value) -> String {
    item.get("id")
        .or_else(|| item.get("item_id"))
        .or_else(|| event.get("item_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "output-{}",
                event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
            )
        })
}

fn delta(event: &Value) -> Option<&str> {
    event.get("delta").and_then(Value::as_str)
}

fn usage(event: &Value) -> Option<Usage> {
    let usage = event.pointer("/response/usage")?;
    Some(Usage {
        prompt_tokens: usage.get("input_tokens")?.as_u64()?,
        total_tokens: usage.get("total_tokens")?.as_u64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ToolCall;

    fn request(messages: Vec<Message>) -> ResponseRequest {
        ResponseRequest::from(CompletionRequest {
            provider: "local".into(),
            model: "model".into(),
            messages,
            temperature: Some(0.5),
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: Some(100),
            stream: true,
            tools: Vec::new(),
        })
    }

    #[test]
    fn builds_stateless_request_and_replays_output_items() {
        let reasoning = json!({
            "id": "rs_1",
            "type": "reasoning",
            "status": "completed",
            "summary": [],
            "encrypted_content": "opaque",
        });
        let function_call = json!({
            "id": "fc_1",
            "type": "function_call",
            "call_id": "call_1",
            "status": "completed",
            "name": "read",
            "arguments": "{\"path\":\"README.md\"}",
        });
        let assistant = Message::assistant_response(
            String::new(),
            "model".into(),
            String::new(),
            vec![ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({"path":"README.md"}),
            }],
            vec![reasoning.clone(), function_call.clone()],
        );
        let value = serde_json::to_value(request(vec![assistant])).unwrap();

        assert_eq!(value["store"], false);
        assert_eq!(value["include"][0], "reasoning.encrypted_content");
        assert_eq!(value["reasoning"]["context"], "all_turns");
        assert_eq!(
            value["input"],
            json!([
                {
                    "id": "rs_1",
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": "opaque",
                },
                {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{\"path\":\"README.md\"}",
                }
            ])
        );
    }

    #[test]
    fn model_switch_rebuilds_portable_assistant_items() {
        let assistant = Message::assistant_response(
            "done".into(),
            "previous-model".into(),
            "private reasoning".into(),
            vec![ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({"path":"README.md"}),
            }],
            vec![
                json!({
                    "id": "rs_1",
                    "type": "reasoning",
                    "status": "completed",
                    "encrypted_content": "model-specific",
                }),
                json!({
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "status": "completed",
                    "name": "read",
                    "arguments": "{}",
                }),
            ],
        );
        let value = serde_json::to_value(request(vec![assistant])).unwrap();

        assert_eq!(
            value["input"],
            json!([
                { "role": "assistant", "content": "done" },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{\"path\":\"README.md\"}",
                }
            ])
        );
    }

    #[test]
    fn converts_tools_and_multimodal_tool_results() {
        let mut request = request(vec![Message::tool(
            "call_1".into(),
            "image".into(),
            Some(ImageContent {
                mime_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
                path: None,
                width: 1,
                height: 1,
            }),
            None,
        )]);
        request.tools.push(ResponseTool {
            r#type: "function",
            name: "read".into(),
            description: "read a file".into(),
            parameters: json!({"type":"object"}),
        });
        let value = serde_json::to_value(request).unwrap();

        assert_eq!(value["tools"][0]["name"], "read");
        assert_eq!(value["input"][0]["type"], "function_call_output");
        assert_eq!(value["input"][0]["output"][1]["type"], "input_image");
    }

    #[test]
    fn decodes_text_reasoning_tools_items_and_usage() {
        let mut decoder = ResponsesDecoder::default();
        assert_eq!(
            decoder
                .parse(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
                .unwrap(),
            vec![ResponseDelta::Text("hello".into())]
        );
        assert_eq!(
            decoder
                .parse(r#"{"type":"response.reasoning_summary_text.delta","delta":"think"}"#)
                .unwrap(),
            vec![ResponseDelta::Reasoning("think".into())]
        );
        let added = decoder
            .parse(r#"{"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":""}}"#)
            .unwrap();
        assert!(matches!(
            &added[0],
            ResponseDelta::ToolCall { index: 0, id: Some(id), .. } if id == "call_1"
        ));
        decoder
            .parse(r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{}"}"#)
            .unwrap();
        let item = decoder
            .parse(r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}}"#)
            .unwrap();
        assert_eq!(
            item,
            vec![ResponseDelta::OutputItem(json!({
                "id": "fc_1",
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{}",
            }))]
        );
        assert_eq!(
            decoder
                .parse(r#"{"type":"response.completed","response":{"usage":{"input_tokens":20,"total_tokens":35}}}"#)
                .unwrap(),
            vec![ResponseDelta::Usage(Usage {
                prompt_tokens: 20,
                total_tokens: 35,
            })]
        );
    }
}
