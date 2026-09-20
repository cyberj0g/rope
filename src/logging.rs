use std::{
    fmt,
    io::{self, Write},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{core::state::Snapshot, runtime::Event};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn init(headless: bool) {
    ENABLED.store(headless, Ordering::Relaxed);
}

pub fn write(level: &str, scope: &str, message: impl fmt::Display) {
    if ENABLED.load(Ordering::Relaxed) {
        let message = message.to_string().replace(['\r', '\n'], " ");
        let _ = writeln!(
            io::stderr().lock(),
            "{} {level} [{scope}] {message}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        );
    }
}

pub(crate) fn event(event: &Event, snapshot: &Snapshot) {
    if ENABLED.load(Ordering::Relaxed)
        && let Some((level, message)) = event_message(event, snapshot)
    {
        write(level, &snapshot.session_id, message);
    }
}

fn event_message(event: &Event, snapshot: &Snapshot) -> Option<(&'static str, String)> {
    let message = match event {
        Event::OperationStarted { id, compacting } => format!(
            "{} started: {id}",
            if *compacting { "compaction" } else { "turn" }
        ),
        Event::ModelRequestStarted(model) => format!("requesting model {model}"),
        Event::ResponseStarted => "model response streaming".into(),
        Event::ModelResponseFinished {
            output_tokens,
            duration,
        } => format!(
            "model response finished: {output_tokens} output tokens in {:.1}s",
            duration.as_secs_f64()
        ),
        Event::ToolStarted { call_id } | Event::ToolResult { call_id, .. } => {
            let block = snapshot.blocks.iter().rev().find(|block| {
                block
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.call_id.as_ref() == Some(call_id))
            })?;
            let name = &block.tool.as_ref()?.name;
            match event {
                Event::ToolResult { success, .. } => {
                    return Some((
                        if *success { "INFO" } else { "ERROR" },
                        format!(
                            "tool {name} ({call_id}) {} in {:.1}s",
                            if *success { "finished" } else { "failed" },
                            block.timer.value().as_secs_f64()
                        ),
                    ));
                }
                _ => format!("tool {name} ({call_id}) started"),
            }
        }
        Event::ApprovalRequested { call, .. } => {
            return Some((
                "WARN",
                format!(
                    "waiting for approval: {} ({}) — approve in a connected client",
                    call.name, call.id
                ),
            ));
        }
        Event::ApprovalResolved { tool, decision, .. } => {
            format!("approval for {tool}: {decision:?}")
        }
        Event::Retrying { seconds } => {
            return Some((
                "WARN",
                format!("model request failed; retrying in {seconds}s"),
            ));
        }
        Event::CompactionStarted => "compacting context".into(),
        Event::ContextCompacted { .. } => "context compacted".into(),
        Event::GenerationFinished { .. } => format!(
            "turn finished; session total: {} tokens",
            snapshot.state.total_tokens
        ),
        Event::GenerationCancelled => "turn cancelled".into(),
        Event::Notice(message) => return Some(("WARN", message.clone())),
        Event::Error(message) => return Some(("ERROR", message.clone())),
        _ => return None,
    };
    Some(("INFO", message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core::state::Projection, runtime::ToolCall};

    #[test]
    fn tool_lifecycle_logs_status_without_arguments_or_output() {
        let mut projection = Projection::new("logs".into());
        let call = ToolCall {
            id: "call-1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "private command"}),
        };
        for event in [
            Event::ToolCallDelta {
                index: 0,
                name: Some(call.name.clone()),
                arguments: call.arguments.to_string(),
            },
            Event::ToolCallFinished {
                index: 0,
                call: call.clone(),
            },
        ] {
            projection.apply(&event);
            assert!(event_message(&event, &projection.snapshot).is_none());
        }
        for (event, level, expected) in [
            (
                Event::ApprovalRequested {
                    approval_id: "approval-1".into(),
                    call,
                },
                "WARN",
                "waiting for approval: shell (call-1)",
            ),
            (
                Event::ToolStarted {
                    call_id: "call-1".into(),
                },
                "INFO",
                "tool shell (call-1) started",
            ),
            (
                Event::ToolResult {
                    call_id: "call-1".into(),
                    output: "private output".into(),
                    success: false,
                    diff: None,
                },
                "ERROR",
                "tool shell (call-1) failed",
            ),
        ] {
            projection.apply(&event);
            let (actual_level, message) = event_message(&event, &projection.snapshot).unwrap();
            assert_eq!(actual_level, level);
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("private"));
        }
        for event in [
            Event::TextDelta("private response".into()),
            Event::ReasoningDelta("private reasoning".into()),
            Event::ToolOutputDelta {
                call_id: "call-1".into(),
                delta: "private output".into(),
            },
        ] {
            assert!(event_message(&event, &projection.snapshot).is_none());
        }
    }
}
