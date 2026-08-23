use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ImageContent {
    pub mime_type: String,
    #[serde(default, skip_serializing)]
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub width: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub height: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageContent>,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        model: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<ImageContent>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl Message {
    pub fn system(content: String) -> Self {
        Self::System { content }
    }
    #[cfg(test)]
    pub fn user(content: String) -> Self {
        Self::User {
            content,
            images: Vec::new(),
        }
    }
    pub fn user_with_images(content: String, images: Vec<ImageContent>) -> Self {
        Self::User { content, images }
    }
    pub fn assistant(
        content: String,
        model: String,
        reasoning: String,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self::Assistant {
            content,
            model,
            reasoning,
            tool_calls,
        }
    }
    pub fn tool(call_id: String, content: String, image: Option<ImageContent>) -> Self {
        Self::Tool {
            call_id,
            content,
            image,
        }
    }

    #[cfg(test)]
    pub fn content(&self) -> &str {
        match self {
            Self::System { content }
            | Self::User { content, .. }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. } => content,
        }
    }
}
