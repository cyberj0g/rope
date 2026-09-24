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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_request: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_request: Option<String>,
        content: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        model: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        response_items: Vec<Value>,
        /// Total time the turn this message answers took, from the user's
        /// prompt to the final response. Set on the turn's final message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// The agent that produced this response. Older messages have none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
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

/// Terminal state of one `subagent` invocation.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Completed,
    /// The child's turn was cancelled by the user; the caller continues.
    UserCancelled,
    Failed,
    /// The process restarted while the invocation was in flight; the
    /// record is reconciled at load time and never restarted.
    Interrupted,
}

/// The structured result of one `subagent` tool call. The control fields
/// (`session_id`, `agent`, `status`) are structured metadata, never inferred
/// from displayed output, and survive the runtime's output limits — only
/// the `response` / `error` text may be truncated.
#[derive(Clone, Debug, Serialize)]
pub struct SubagentOutcome {
    pub session_id: Option<String>,
    pub agent: String,
    pub status: SubagentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

impl SubagentOutcome {
    pub fn new(agent: impl Into<String>, status: SubagentStatus) -> Self {
        Self {
            session_id: None,
            agent: agent.into(),
            status,
            response: None,
            error: None,
            message: None,
            tokens: None,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(
            self.status,
            SubagentStatus::Failed | SubagentStatus::Interrupted
        )
    }

    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

/// Shrinks a `subagent` result to at most `max_bytes` while keeping it valid
/// JSON: the long text fields are cut (keeping the tail) until the control
/// fields plus whatever fits remain. Returns the input unchanged when it is
/// not a JSON object.
pub fn bounded_subagent_json(json: &str, max_bytes: usize) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(json) else {
        return json.to_owned();
    };
    let total = |value: &Value| {
        serde_json::to_string(value)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
    };
    if total(&value) <= max_bytes {
        return json.to_owned();
    }
    let string_len = |text: &str| serde_json::to_string(text).map(|v| v.len()).unwrap_or(0);
    for field in ["response", "error", "message"] {
        // Phase 1: pull the field's text out (ends the mutable borrow).
        let Some(original) = ({
            let object = match value.as_object_mut() {
                Some(object) => object,
                None => break,
            };
            match object.get_mut(field) {
                Some(Value::String(text)) => Some(std::mem::take(text)),
                _ => None,
            }
        }) else {
            continue;
        };
        // Phase 2: compute the serialized size of the document with this
        // field set to "", so we can budget the remaining bytes.
        let base = total(&value).saturating_sub(string_len(""));
        let fits =
            |chars: &[char]| base + string_len(&chars.iter().collect::<String>()) <= max_bytes;
        let mut chars: Vec<char> = original.chars().collect();
        while !fits(&chars) && chars.len() > 24 {
            let half = chars.len() / 2;
            chars.drain(half.saturating_sub(12)..half + 12);
        }
        if !fits(&chars) {
            chars.truncate(24);
        }
        if !chars.is_empty() {
            chars.insert(0, '…');
        }
        while !fits(&chars) && chars.len() > 1 {
            chars.pop();
        }
        // Phase 3: write the (possibly truncated) text back.
        if let Some(object) = value.as_object_mut()
            && let Some(Value::String(text)) = object.get_mut(field)
        {
            *text = chars.into_iter().collect();
        }
        if total(&value) <= max_bytes {
            break;
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| json.to_owned())
}

impl Message {
    pub fn system(content: String) -> Self {
        Self::System {
            content,
            raw_request: None,
        }
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
            raw_request: None,
            content,
            model,
            reasoning,
            tool_calls,
            response_items: Vec::new(),
            duration_ms: None,
            agent: None,
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
            raw_request: None,
            content,
            model,
            reasoning,
            tool_calls,
            response_items,
            duration_ms: None,
            agent: None,
        }
    }
    /// Tags the response with the agent that produced it, for historical
    /// attribution across agent switches.
    pub fn with_agent(mut self, agent: String) -> Self {
        if let Self::Assistant { agent: slot, .. } = &mut self {
            *slot = Some(agent);
        }
        self
    }
    pub fn with_raw_request(mut self, id: Option<String>) -> Self {
        match &mut self {
            Self::System { raw_request, .. } | Self::Assistant { raw_request, .. } => {
                *raw_request = id
            }
            _ => unreachable!(),
        }
        self
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
            Self::System { content, .. }
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
            Message::Assistant {
                response_items,
                duration_ms,
                ..
            } if response_items.is_empty() && duration_ms.is_none()
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
