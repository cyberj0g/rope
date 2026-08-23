use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
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
    pub fn user(content: String) -> Self {
        Self::User { content }
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
    pub fn tool(call_id: String, content: String) -> Self {
        Self::Tool { call_id, content }
    }

    #[cfg(test)]
    pub fn content(&self) -> &str {
        match self {
            Self::System { content }
            | Self::User { content }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. } => content,
        }
    }
}
