use serde::{Deserialize, Serialize};

use crate::{
    runtime::{ApprovalDecision, ReasoningEffort},
    session::SessionInfo,
};

pub const VERSION: u32 = 1;
pub const MAX_COMMAND_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    SendMessage {
        content: String,
        #[serde(default)]
        attachments: Vec<String>,
    },
    Cancel {
        turn_id: String,
    },
    Approve {
        turn_id: String,
        approval_id: String,
        decision: ApprovalDecision,
    },
    SetModel {
        model: String,
        revision: u64,
    },
    SetReasoning {
        effort: Option<ReasoningEffort>,
        revision: u64,
    },
    Compact,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    CreateSession { name: Option<String> },
    Subscribe { session_id: String },
    Unsubscribe { session_id: String },
    Command { session_id: String, action: Action },
    GitDiff { path: Option<String> },
}

#[derive(Debug, Deserialize)]
pub struct ClientRequest {
    pub request_id: String,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol: u32,
    pub token: String,
    pub client_id: Option<String>,
    pub server_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Error {
    pub code: String,
    pub message: String,
}

impl Error {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Accepted {
    pub turn_id: Option<String>,
    pub settings_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogEntry {
    #[serde(flatten)]
    pub info: SessionInfo,
    pub project_root: Option<std::path::PathBuf>,
    pub total_tokens: u64,
    pub total_cost: Option<f64>,
    pub activity: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogSnapshot {
    pub seq: u64,
    pub sessions: Vec<CatalogEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_decode_flattened_envelopes_and_reject_unknown_payload_fields() {
        let request: ClientRequest =
            serde_json::from_str(r#"{"request_id":"1","type":"subscribe","session_id":"demo"}"#)
                .unwrap();
        assert_eq!(request.request_id, "1");
        assert!(
            matches!(request.request, Request::Subscribe { session_id } if session_id == "demo")
        );
        assert!(
            serde_json::from_str::<ClientRequest>(
                r#"{"request_id":"1","type":"subscribe","session_id":"demo","extra":true}"#,
            )
            .is_err()
        );
    }
}
