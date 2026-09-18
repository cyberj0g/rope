use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::{
    provider::{Provider, ResponseStream},
    runtime::CompletionRequest,
};

pub struct RecordingProvider {
    pub inner: Arc<dyn Provider>,
    pub directory: PathBuf,
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream> {
        self.inner.stream(request).await
    }

    async fn record_request(&self, request: &CompletionRequest) -> Result<Option<String>> {
        let Some(mut body) = self.inner.request_body(request.clone())? else {
            return Ok(None);
        };
        truncate_binary(&mut body, "");
        let id = uuid::Uuid::new_v4().to_string();
        let directory = self.directory.join("requests");
        tokio::fs::create_dir_all(&directory).await?;
        tokio::fs::write(
            directory.join(format!("{id}.json")),
            serde_json::to_vec(&body)?,
        )
        .await?;
        Ok(Some(id))
    }
}

fn truncate_binary(value: &mut Value, key: &str) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                truncate_binary(value, key);
            }
        }
        Value::Array(items) => {
            for value in items {
                truncate_binary(value, key);
            }
        }
        Value::String(text) => {
            let start =
                if matches!(key, "url" | "image_url" | "file_data") && text.starts_with("data:") {
                    text.find(";base64,").map(|i| i + 8)
                } else if matches!(key, "encrypted_content" | "b64_json" | "audio_data") {
                    Some(0)
                } else {
                    None
                };
            if let Some(start) = start {
                let length = text.len() - start;
                if length > 80 {
                    let end = text
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|&i| i >= start + 80)
                        .unwrap_or(text.len());
                    text.truncate(end);
                    text.push_str(&format!("… [truncated: {length} encoded bytes]"));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, ProviderApi},
        provider::openai::OpenAiProvider,
        runtime::{FileContent, ImageContent, Message},
    };
    use serde_json::json;

    #[tokio::test]
    async fn records_both_wire_formats_without_loading_delivered_files() {
        for api in [ProviderApi::Responses, ProviderApi::ChatCompletions] {
            let mut config = Config::default();
            config.providers.push(crate::config::ProviderConfig {
                name: config.provider_name().into(),
                base_url: String::new(),
                api_key: String::new(),
                api,
            });
            let provider = OpenAiProvider::from_config(&config);
            let directory = tempfile::tempdir().unwrap();
            let request = CompletionRequest {
                provider: config.provider_name().into(),
                model: "test".into(),
                messages: vec![
                    Message::system("instructions".repeat(1000)),
                    Message::user_with_images(
                        "inspect".into(),
                        vec![ImageContent {
                            mime_type: "image/png".into(),
                            data: "A".repeat(1000),
                            path: None,
                            width: 1,
                            height: 1,
                        }],
                    ),
                    Message::tool_file(
                        "call".into(),
                        "delivery confirmed".into(),
                        FileContent {
                            path: "/does/not/exist".into(),
                            name: "secret.txt".into(),
                            size: 123,
                            mime_type: "text/plain".into(),
                        },
                        None,
                    ),
                ],
                temperature: Some(0.25),
                reasoning_effort: None,
                max_tokens: Some(1024),
                stream: true,
                tools: Vec::new(),
            };
            let expected = provider.request_body(request.clone()).unwrap().unwrap();
            let recorder = RecordingProvider {
                inner: Arc::new(provider),
                directory: directory.path().into(),
            };
            let id = recorder.record_request(&request).await.unwrap().unwrap();
            let body: Value = serde_json::from_slice(
                &tokio::fs::read(directory.path().join("requests").join(format!("{id}.json")))
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(body["model"], expected["model"]);
            assert_eq!(body["temperature"], expected["temperature"]);
            let serialized = body.to_string();
            assert!(serialized.contains(&"instructions".repeat(1000)));
            assert!(serialized.contains("data:image/png;base64,AAAAAAAA"));
            assert!(serialized.contains("truncated: 1000 encoded bytes"));
            assert!(!serialized.contains(&"A".repeat(1000)));
            assert!(!serialized.contains("secret.txt"));
            assert!(!serialized.contains("/does/not/exist"));
            assert!(serialized.contains("delivery confirmed"));
            let mut sanitized = expected;
            truncate_binary(&mut sanitized, "");
            assert_eq!(body, sanitized);
        }
    }

    #[test]
    fn truncates_opaque_values_but_never_long_readable_text() {
        let mut body = json!({"encrypted_content": "a".repeat(2000), "text": "a".repeat(10000)});
        truncate_binary(&mut body, "");
        assert!(body["encrypted_content"].as_str().unwrap().len() < 150);
        assert_eq!(body["text"].as_str().unwrap().len(), 10000);
    }
}
