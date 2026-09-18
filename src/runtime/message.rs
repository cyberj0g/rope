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

pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// A file a tool published to the chat for the user: images render inline,
/// everything else appears as a downloadable file tile. The reference names
/// the file on disk — its bytes never enter the model context.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FileContent {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
}

/// A small extension-based content type for files served to the client.
pub fn guess_mime_type(path: &std::path::Path) -> String {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "md" | "markdown" | "org" | "rst" => "text/markdown",
        "html" | "htm" => "text/html",
        "csv" | "tsv" => "text/csv",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    };
    mime.to_owned()
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
    /// A steering message sent while a turn is already in progress. Sent to
    /// the model with the user role at the next model request.
    Steer {
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        response_items: Vec<Value>,
    },
    Tool {
        call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<ImageContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<FileContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
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
    pub fn steer(content: String, images: Vec<ImageContent>) -> Self {
        Self::Steer { content, images }
    }
    #[cfg(test)]
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
            response_items: Vec::new(),
        }
    }
    pub fn assistant_response(
        content: String,
        model: String,
        reasoning: String,
        tool_calls: Vec<ToolCall>,
        response_items: Vec<Value>,
    ) -> Self {
        Self::Assistant {
            content,
            model,
            reasoning,
            tool_calls,
            response_items,
        }
    }
    pub fn tool(
        call_id: String,
        content: String,
        image: Option<ImageContent>,
        diff: Option<String>,
    ) -> Self {
        Self::Tool {
            call_id,
            content,
            image,
            file: None,
            diff,
        }
    }

    /// A tool result that published a file to the chat. The file never
    /// enters the model context; only the text content does.
    pub fn tool_file(
        call_id: String,
        content: String,
        file: FileContent,
        diff: Option<String>,
    ) -> Self {
        Self::Tool {
            call_id,
            content,
            image: None,
            file: Some(file),
            diff,
        }
    }

    /// The images attached to this message: user- or steer-attached or a
    /// tool result.
    pub fn images(&self) -> &[ImageContent] {
        match self {
            Self::User { images, .. } | Self::Steer { images, .. } => images,
            Self::Tool {
                image: Some(image), ..
            } => std::slice::from_ref(image),
            _ => &[],
        }
    }

    #[cfg(test)]
    pub fn content(&self) -> &str {
        match self {
            Self::System { content }
            | Self::User { content, .. }
            | Self::Steer { content, .. }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. } => content,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_assistant_messages_default_response_items() {
        let message: Message = serde_json::from_str(
            r#"{"role":"assistant","content":"hello","model":"old","reasoning":"","tool_calls":[]}"#,
        )
        .unwrap();

        assert!(matches!(
            message,
            Message::Assistant { response_items, .. } if response_items.is_empty()
        ));
    }

    #[test]
    fn steer_messages_round_trip_through_their_own_role() {
        let steer = Message::steer("stay focused".into(), Vec::new());
        let encoded = serde_json::to_string(&steer).unwrap();

        assert!(encoded.starts_with(r#"{"role":"steer""#));
        assert_eq!(serde_json::from_str::<Message>(&encoded).unwrap(), steer);
    }

    #[test]
    fn tool_files_round_trip_and_default_in_old_messages() {
        let file = FileContent {
            path: "/tmp/report.png".into(),
            name: "report.png".into(),
            size: 12,
            mime_type: "image/png".into(),
        };
        let message = Message::tool_file(
            "call_1".into(),
            "sent /tmp/report.png (12 bytes)".into(),
            file.clone(),
            None,
        );
        let encoded = serde_json::to_string(&message).unwrap();
        assert!(encoded.contains(r#""file""#));
        assert_eq!(serde_json::from_str::<Message>(&encoded).unwrap(), message);

        // A message written before send_file existed has no `file` field.
        let legacy = r#"{"role":"tool","call_id":"c","content":"old","image":null,"diff":null}"#;
        let legacy: Message = serde_json::from_str(legacy).unwrap();
        assert!(matches!(legacy, Message::Tool { file: None, .. }));
    }
}
