mod support;

use rope::{
    core::state::{BlockKind, ToolStatus},
    protocol::Action,
    provider::ResponseDelta,
    runtime::{ApprovalDecision, Event, Message},
};
use serde_json::json;
use support::{Harness, prompt, until};

#[tokio::test]
async fn legacy_sessions_bind_on_open_and_foreign_sessions_are_rejected() {
    use rope::{config::Config, core::Core, session::Session};
    use std::sync::Arc;
    let harness = Harness::new().await;
    let legacy = Session::create_in(harness.storage.path().into(), "legacy".into())
        .await
        .unwrap();
    legacy
        .append(&[Message::assistant_response(
            "visible".into(),
            "model".into(),
            String::new(),
            vec![],
            vec![json!({"encrypted_content":"private-provider-replay"})],
        )])
        .await
        .unwrap();
    drop(legacy);
    let snapshot = harness.core.subscribe("legacy").await.unwrap().snapshot;
    assert!(
        snapshot
            .blocks
            .iter()
            .any(|block| block.content == "visible")
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("private-provider-replay")
    );
    harness.core.shutdown().await.unwrap();
    let project = tempfile::tempdir().unwrap();
    let (sender, _requests) = tokio::sync::mpsc::unbounded_channel();
    let foreign = Core::new(
        Config::default(),
        project.path().into(),
        harness.storage.path().into(),
        Arc::new(support::ControlledProvider(sender)),
    )
    .await
    .unwrap();
    assert!(foreign.subscribe_catalog().0.sessions.is_empty());
    assert!(
        foreign
            .subscribe("legacy")
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("another project")
    );
    foreign.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_turns_preserve_partial_work_with_a_failure_marker() {
    let mut harness = Harness::new().await;
    let id = harness.core.create(Some("failure".into())).await.unwrap();
    let mut subscription = harness.core.subscribe(&id).await.unwrap();
    harness.core.command(&id, prompt("work")).await.unwrap();
    let request = harness.next().await;
    request
        .stream
        .send(Ok(ResponseDelta::Text("partial answer".into())))
        .unwrap();
    request
        .stream
        .send(Err(anyhow::anyhow!("provider stream failed")))
        .unwrap();
    drop(request);
    until(&mut subscription, |event| matches!(event, Event::Error(_))).await;
    let snapshot = harness.core.subscribe(&id).await.unwrap().snapshot;
    assert_eq!(snapshot.state.phase, "error");
    assert!(
        snapshot
            .blocks
            .iter()
            .any(|block| block.content == "partial answer")
    );
    harness.core.shutdown().await.unwrap();
    let (_, messages) = rope::session::Session::resume_in(harness.storage.path().into(), &id)
        .await
        .unwrap();
    assert!(messages.iter().any(
        |m| matches!(m, Message::System { content, .. } if content.starts_with("turn failed:"))
    ));
    assert!(
        !messages.iter().any(
            |m| matches!(m, Message::System { content, .. } if content == "cancelled by user")
        )
    );
}

#[tokio::test]
async fn shared_sessions_accept_concurrent_messages_and_snapshot_the_live_tail() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let id = core.create(Some("shared".into())).await.unwrap();
    let mut a = core.subscribe(&id).await.unwrap();
    let mut b = core.subscribe(&id).await.unwrap();
    let (first, second) = tokio::join!(
        core.command(&id, prompt("one")),
        core.command(&id, prompt("two"))
    );
    assert_eq!(first.unwrap().turn_id, second.unwrap().turn_id);
    let request = harness.next().await;
    request
        .stream
        .send(Ok(ResponseDelta::Reasoning("thought".into())))
        .unwrap();
    request
        .stream
        .send(Ok(ResponseDelta::Text("partial".into())))
        .unwrap();
    until(
        &mut a,
        |event| matches!(event, Event::TextDelta(text) if text == "partial"),
    )
    .await;
    until(
        &mut b,
        |event| matches!(event, Event::TextDelta(text) if text == "partial"),
    )
    .await;
    let late = core.subscribe(&id).await.unwrap().snapshot;
    assert!(late.state.turn_id.is_some());
    assert!(
        late.blocks
            .iter()
            .any(|b| b.kind == BlockKind::Thinking && b.content == "thought")
    );
    assert!(
        late.blocks
            .iter()
            .any(|b| b.kind == BlockKind::Assistant && b.content == "partial")
    );
    assert_eq!(
        late.blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::User | BlockKind::Steer))
            .count(),
        2
    );
    drop(a);
    drop(b);
    request.tool("list_files", json!({}));
    let continuation = harness.next().await;
    assert!(
        continuation
            .request
            .messages
            .iter()
            .any(|m| matches!(m, Message::Steer { .. }))
    );
    let mut reconnected = core.subscribe(&id).await.unwrap();
    continuation.finish("done");
    until(&mut reconnected, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    let snapshot = core.subscribe(&id).await.unwrap().snapshot;
    assert!(snapshot.state.turn_id.is_none());
    assert_eq!(snapshot.state.queued_steers, 0);
    assert_eq!(
        snapshot
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::User)
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Steer)
            .count(),
        1
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn independent_sessions_run_concurrently_and_old_cancels_are_rejected() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let a = core.create(Some("a".into())).await.unwrap();
    let b = core.create(Some("b".into())).await.unwrap();
    let mut events_b = core.subscribe(&b).await.unwrap();
    let old = core
        .command(&a, prompt("alpha"))
        .await
        .unwrap()
        .turn_id
        .unwrap();
    let request_a = harness.next().await;
    let turn_b = core
        .command(&b, prompt("beta"))
        .await
        .unwrap()
        .turn_id
        .unwrap();
    let request_b = harness.next().await;
    core.command(
        &a,
        Action::Cancel {
            turn_id: old.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        core.subscribe(&b)
            .await
            .unwrap()
            .snapshot
            .state
            .turn_id
            .as_deref(),
        Some(turn_b.as_str())
    );
    let new = core
        .command(&a, prompt("again"))
        .await
        .unwrap()
        .turn_id
        .unwrap();
    assert_ne!(new, old);
    let error = core
        .command(&a, Action::Cancel { turn_id: old })
        .await
        .unwrap_err();
    assert_eq!(error.code, "stale_turn");
    let new_request = harness.next().await;
    assert!(request_a.stream.is_closed());
    request_b.finish("beta done");
    until(&mut events_b, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    assert_eq!(
        core.subscribe(&a).await.unwrap().snapshot.state.turn_id,
        Some(new)
    );
    drop(new_request);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_is_shared_and_resolved_once() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let id = core.create(Some("approval".into())).await.unwrap();
    let mut a = core.subscribe(&id).await.unwrap();
    let turn_id = core
        .command(&id, prompt("write a file"))
        .await
        .unwrap()
        .turn_id
        .unwrap();
    harness
        .next()
        .await
        .tool("write", json!({"path":"out.txt","content":"once"}));
    until(&mut a, |event| {
        matches!(event, Event::ApprovalRequested { .. })
    })
    .await;
    let b = core.subscribe(&id).await.unwrap();
    let approval = b.snapshot.state.approval.unwrap();
    assert!(b.snapshot.blocks.iter().any(|block| {
        block
            .tool
            .as_ref()
            .is_some_and(|tool| tool.status == ToolStatus::WaitingApproval)
    }));
    let action = Action::Approve {
        turn_id,
        approval_id: approval.id,
        decision: ApprovalDecision::AllowOnce,
    };
    let (one, two) = tokio::join!(core.command(&id, action.clone()), core.command(&id, action));
    assert_ne!(one.is_ok(), two.is_ok());
    assert_eq!(one.err().or(two.err()).unwrap().code, "stale_approval");
    let next = harness.next().await;
    assert_eq!(
        std::fs::read_to_string(harness.project.path().join("out.txt")).unwrap(),
        "once"
    );
    assert!(
        core.subscribe(&id)
            .await
            .unwrap()
            .snapshot
            .state
            .approval
            .is_none()
    );
    next.finish("done");
    until(&mut a, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_response_cut_off_at_the_context_limit_compacts_and_continues() {
    let mut config = rope::config::Config::default();
    config.models[0].max_context_tokens = 8_192;
    let mut harness = Harness::with_config(config).await;
    let core = harness.core.clone();
    let id = core.create(Some("truncated".into())).await.unwrap();
    let mut subscription = core.subscribe(&id).await.unwrap();
    core.command(&id, prompt("finish the work")).await.unwrap();
    let request = harness.next().await;
    for delta in [
        ResponseDelta::Text("work so far".into()),
        ResponseDelta::Usage(rope::provider::Usage {
            prompt_tokens: 6_000,
            total_tokens: 8_192,
        }),
        ResponseDelta::Truncated("max_output_tokens".into()),
    ] {
        request.stream.send(Ok(delta)).unwrap();
    }
    drop(request);
    let summary = harness.next().await;
    assert!(summary.request.tools.is_empty());
    summary.finish("work remains to be done");
    let continuation = harness.next().await;
    assert!(continuation.request.messages.iter().any(|message| {
        matches!(message, Message::System { content, .. } if content.contains("work remains to be done"))
    }));
    continuation.finish("finished");
    until(&mut subscription, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn mid_turn_compaction_preserves_steering_and_the_saved_boundary() {
    for cancel in [false, true] {
        let mut config = rope::config::Config::default();
        config.models[0].max_context_tokens = 8_192;
        let mut harness = Harness::with_config(config).await;
        std::fs::write(harness.project.path().join("note.txt"), "completed work").unwrap();
        let core = harness.core.clone();
        let id = core.create(Some("mid-turn".into())).await.unwrap();
        let mut subscription = core.subscribe(&id).await.unwrap();
        let turn_id = core
            .command(&id, prompt("read the note and continue"))
            .await
            .unwrap()
            .turn_id
            .unwrap();
        let request = harness.next().await;
        request
            .stream
            .send(Ok(ResponseDelta::Usage(rope::provider::Usage {
                prompt_tokens: 6_000,
                total_tokens: 6_500,
            })))
            .unwrap();
        request.tool("read", json!({"path": "note.txt"}));

        let summary = harness.next().await;
        assert!(
            summary.request.tools.is_empty(),
            "compact before asking the model to continue"
        );
        assert!(summary.request.messages.iter().any(|message| {
            matches!(message, Message::Tool { content, .. } if content.contains("completed work"))
        }));
        core.command(&id, prompt("also check the tests"))
            .await
            .unwrap();
        summary.finish("the note has been read; keep working");

        let continuation = harness.next().await;
        assert!(continuation.request.messages.iter().any(|message| {
            matches!(message, Message::Steer { content, .. }
                if content == "also check the tests")
        }));
        assert!(
            !continuation
                .request
                .messages
                .iter()
                .any(|message| matches!(message, Message::Tool { .. }))
        );
        if cancel {
            core.command(&id, Action::Cancel { turn_id }).await.unwrap();
            until(&mut subscription, |event| {
                matches!(event, Event::GenerationCancelled)
            })
            .await;
            assert!(continuation.stream.is_closed());
        } else {
            continuation.finish("finished");
            until(&mut subscription, |event| {
                matches!(event, Event::GenerationFinished { .. })
            })
            .await;
        }
        core.shutdown().await.unwrap();

        let (session, messages) =
            rope::session::Session::resume_in(harness.storage.path().into(), &id)
                .await
                .unwrap();
        assert_eq!(
            session.meta.compaction_summary.as_deref(),
            Some("the note has been read; keep working")
        );
        assert_eq!(session.meta.compacted_through, 4);
        assert!(matches!(&messages[1], Message::User { content, .. }
                if content.starts_with("read the note and continue\n\n<runtime-context>")));
        assert!(matches!(&messages[3], Message::Tool { .. }));
        let remaining = &messages[session.meta.compacted_through..];
        assert!(matches!(&remaining[0], Message::Steer { content, .. }
                if content == "also check the tests"));
        assert_eq!(remaining.len(), 2);
        if cancel {
            assert!(
                matches!(&remaining[1], Message::System { content, .. } if content == "cancelled by user")
            );
        } else {
            assert!(
                matches!(&remaining[1], Message::Assistant { content, .. } if content == "finished")
            );
        }
    }
}

#[tokio::test]
async fn manual_compaction_stays_responsive_and_can_be_cancelled() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let id = core.create(Some("compact".into())).await.unwrap();
    let mut a = core.subscribe(&id).await.unwrap();
    core.command(&id, prompt("first")).await.unwrap();
    harness.next().await.finish("answer");
    until(&mut a, |e| matches!(e, Event::GenerationFinished { .. })).await;
    let turn_id = core
        .command(&id, Action::Compact)
        .await
        .unwrap()
        .turn_id
        .unwrap();
    let request = harness.next().await;
    assert!(core.subscribe(&id).await.unwrap().snapshot.state.compacting);
    assert_eq!(
        core.command(&id, prompt("wait")).await.unwrap_err().code,
        "busy"
    );
    core.command(&id, Action::Cancel { turn_id }).await.unwrap();
    assert!(
        core.subscribe(&id)
            .await
            .unwrap()
            .snapshot
            .state
            .turn_id
            .is_none()
    );
    assert!(request.stream.is_closed());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn catalog_is_shared_and_session_creation_and_writes_are_exclusive() {
    let harness = Harness::new().await;
    let core = &harness.core;
    let (_, mut catalog) = core.subscribe_catalog();
    let (a, b) = tokio::join!(core.create(None), core.create(None));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a, b);
    assert!(
        catalog
            .recv()
            .await
            .unwrap()
            .sessions
            .iter()
            .any(|s| s.info.name == a || s.info.name == b)
    );
    assert_eq!(core.subscribe_catalog().0.sessions.len(), 2);
    assert!(rope::session::lock_session(&harness.storage.path().join(&a)).is_err());
    core.shutdown().await.unwrap();
    rope::session::lock_session(&harness.storage.path().join(&a)).unwrap();
}

#[tokio::test]
async fn settings_are_scoped_revisioned_and_saved_in_the_session() {
    let harness = Harness::new().await;
    let core = &harness.core;
    let a = core.create(Some("a".into())).await.unwrap();
    let b = core.create(Some("b".into())).await.unwrap();
    let before = core.subscribe(&b).await.unwrap().snapshot;
    let model = core.models()[0].name.clone();
    core.command(
        &a,
        Action::SetModel {
            model: model.clone(),
            revision: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        core.subscribe(&a)
            .await
            .unwrap()
            .snapshot
            .state
            .settings_revision,
        1
    );
    assert_eq!(
        core.subscribe(&b)
            .await
            .unwrap()
            .snapshot
            .state
            .settings_revision,
        before.state.settings_revision
    );
    assert_eq!(
        core.command(&a, Action::SetModel { model, revision: 0 })
            .await
            .unwrap_err()
            .code,
        "stale_settings"
    );
    let meta: rope::session::SessionMeta = serde_json::from_slice(
        &std::fs::read(harness.storage.path().join("a/session.json")).unwrap(),
    )
    .unwrap();
    assert!(meta.settings.is_some());
    assert_eq!(meta.project_root.as_deref(), Some(harness.project.path()));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_subscribers_do_not_block_the_runtime_and_can_resnapshot() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let id = core.create(Some("slow".into())).await.unwrap();
    let mut slow = core.subscribe(&id).await.unwrap();
    let mut fast = core.subscribe(&id).await.unwrap();
    core.command(&id, prompt("stream")).await.unwrap();
    let request = harness.next().await;
    let reading = tokio::spawn(async move {
        until(&mut fast, |event| {
            matches!(event, Event::GenerationFinished { .. })
        })
        .await
    });
    for _ in 0..500 {
        request
            .stream
            .send(Ok(ResponseDelta::Text("x".into())))
            .unwrap();
        tokio::task::yield_now().await;
    }
    request.finish("end");
    reading.await.unwrap();
    assert!(matches!(
        slow.updates.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    let fresh = core.subscribe(&id).await.unwrap().snapshot;
    assert!(
        fresh
            .blocks
            .iter()
            .any(|block| block.content == format!("{}end", "x".repeat(500)))
    );
    core.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_one_session_does_not_kill_another_sessions_shell_job() {
    let mut config = rope::config::Config::default();
    config.tools.shell = rope::tool::Approval::Allow;
    let mut harness = Harness::with_config(config).await;
    let core = harness.core.clone();
    let a = core.create(Some("a".into())).await.unwrap();
    let b = core.create(Some("b".into())).await.unwrap();
    let turn_a = core
        .command(&a, prompt("alpha"))
        .await
        .unwrap()
        .turn_id
        .unwrap();
    harness
        .next()
        .await
        .tool("shell", json!({"command":"sleep 30","yield_time_ms":1}));
    let waiting_a = harness.next().await;
    core.command(&b, prompt("beta")).await.unwrap();
    harness
        .next()
        .await
        .tool("shell", json!({"command":"sleep 30","yield_time_ms":1}));
    let waiting_b = harness.next().await;
    let job_id = waiting_b
        .request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Tool { content, .. } => content
                .lines()
                .find_map(|line| line.strip_prefix("job_id: "))
                .map(str::to_owned),
            _ => None,
        })
        .unwrap();
    core.command(&a, Action::Cancel { turn_id: turn_a })
        .await
        .unwrap();
    assert!(waiting_a.stream.is_closed());
    waiting_b.tool("shell_poll", json!({"job_id":job_id,"yield_time_ms":1}));
    let next = harness.next().await;
    let output = next
        .request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Tool { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    assert!(output.starts_with("status: running"), "{output}");
    drop(next);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_requests_are_lazy_historical_and_survive_compaction_and_restart() {
    use rope::{config::Config, core::Core};
    use std::sync::Arc;
    let mut harness = Harness::new().await;
    let id = harness.core.create(Some("raw".into())).await.unwrap();
    let mut subscription = harness.core.subscribe(&id).await.unwrap();
    harness
        .core
        .command(&id, prompt("first prompt"))
        .await
        .unwrap();
    harness.next().await.tool("list_files", json!({"path":"."}));
    harness.next().await.finish("first answer");
    until(&mut subscription, |e| {
        matches!(e, Event::GenerationFinished { .. })
    })
    .await;
    let first = harness.core.subscribe(&id).await.unwrap().snapshot;
    let raw = harness
        .core
        .raw_request(&id, &first.blocks[0].id)
        .await
        .unwrap();
    assert_eq!(
        raw,
        harness
            .core
            .raw_request(&id, &first.blocks[1].id)
            .await
            .unwrap()
    );
    assert!(raw.to_string().contains("first prompt"));
    let final_raw = harness
        .core
        .raw_request(&id, &first.blocks.last().unwrap().id)
        .await
        .unwrap();
    assert_ne!(raw, final_raw);
    assert!(final_raw.to_string().contains("function_call_output"));
    assert!(raw.get("tools").is_some());
    let snapshot_json = serde_json::to_value(&first).unwrap();
    assert!(
        snapshot_json["blocks"][0]
            .get("raw_request")
            .unwrap()
            .is_string()
    );
    assert!(snapshot_json["blocks"][0].get("body").is_none());
    assert!(!snapshot_json.to_string().contains("parameters"));
    assert!(harness.core.raw_request(&id, "../requests").await.is_err());

    harness.core.command(&id, Action::Compact).await.unwrap();
    harness.next().await.finish("summary of first prompt");
    until(&mut subscription, |e| {
        matches!(e, Event::GenerationFinished { .. })
    })
    .await;
    let compacted = harness.core.subscribe(&id).await.unwrap().snapshot;
    let summary_raw = harness
        .core
        .raw_request(&id, &compacted.blocks.last().unwrap().id)
        .await
        .unwrap();
    assert!(
        summary_raw
            .to_string()
            .contains("Write the continuation summary")
    );

    let accepted = harness
        .core
        .command(&id, prompt("second prompt"))
        .await
        .unwrap();
    let pending = harness.next().await;
    harness
        .core
        .command(
            &id,
            Action::Cancel {
                turn_id: accepted.turn_id.unwrap(),
            },
        )
        .await
        .unwrap();
    drop(pending);
    until(&mut subscription, |e| {
        matches!(e, Event::GenerationCancelled)
    })
    .await;
    let cancelled = harness.core.subscribe(&id).await.unwrap().snapshot;
    let second = cancelled
        .blocks
        .iter()
        .find(|b| b.content.starts_with("second prompt"))
        .unwrap();
    let second_raw = harness.core.raw_request(&id, &second.id).await.unwrap();
    assert!(second_raw.to_string().contains("summary of first prompt"));
    assert!(second_raw.to_string().contains("second prompt"));
    assert_eq!(
        raw,
        harness
            .core
            .raw_request(&id, &first.blocks[0].id)
            .await
            .unwrap()
    );
    harness.core.shutdown().await.unwrap();

    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let reopened = Core::new(
        Config::default(),
        harness.project.path().into(),
        harness.storage.path().into(),
        Arc::new(support::ControlledProvider(sender)),
    )
    .await
    .unwrap();
    let restored = reopened.subscribe(&id).await.unwrap().snapshot;
    for (content, expected) in [
        ("first prompt", raw),
        ("context compacted", summary_raw),
        ("second prompt", second_raw),
    ] {
        let block = restored
            .blocks
            .iter()
            .find(|b| b.content.starts_with(content))
            .unwrap();
        assert_eq!(
            expected,
            reopened.raw_request(&id, &block.id).await.unwrap(),
            "{content}"
        );
    }
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagent_spawn_releases_the_child_creation_lock_before_loading() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let id = core.create(Some("parent".into())).await.unwrap();
    let mut subscription = core.subscribe(&id).await.unwrap();
    core.command(&id, prompt("delegate")).await.unwrap();
    // The parent's first turn emits the delegation call.
    harness.next().await.tool(
        "subagent",
        json!({"agent": "assistant", "prompt": "do the thing"}),
    );
    // The core spawns the child and starts its delegated turn.
    harness.next().await.finish("child done");
    // The parent receives the structured result and continues.
    harness.next().await.finish("parent done");
    until(&mut subscription, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    let snapshot = core.subscribe(&id).await.unwrap().snapshot;
    let tool = snapshot
        .blocks
        .iter()
        .find(|block| {
            block
                .tool
                .as_ref()
                .is_some_and(|tool| tool.name == "subagent")
        })
        .and_then(|block| block.tool.clone())
        .expect("subagent tool block");
    assert_eq!(tool.status, ToolStatus::Done, "output: {:?}", tool.output);
    let output = tool.output.as_deref().unwrap_or_default();
    // Regression: the child's creation lock used to stay held while the core
    // re-locked the same session, so the spawn failed with a lock error.
    assert!(
        !output.contains("already owned by another Rope process"),
        "subagent spawn hit the session lock: {output}"
    );
    let outcome: serde_json::Value = serde_json::from_str(output).unwrap();
    assert_eq!(outcome["status"], "completed");
    // A child session was actually created on disk.
    let children = std::fs::read_dir(harness.storage.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("sub-"))
        .count();
    assert_eq!(children, 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagent_only_definition_runs_in_its_child_session() {
    use rope::{config::Config, core::Core};
    use std::sync::Arc;

    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let agents = project.path().join(".rope/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("review.md"),
        "+++\ndescription = \"Review code\"\nmode = \"subagent\"\n+++\nReview the code carefully.\n",
    )
    .unwrap();
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let core = Core::new(
        Config::default(),
        project.path().into(),
        storage.path().into(),
        Arc::new(support::ControlledProvider(sender)),
    )
    .await
    .unwrap();
    let parent = core.create(Some("parent".into())).await.unwrap();
    let mut subscription = core.subscribe(&parent).await.unwrap();
    core.command(&parent, prompt("delegate review"))
        .await
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    first.tool(
        "subagent",
        json!({"agent": "review", "prompt": "Review this"}),
    );
    let child = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .expect("subagent-only child must reach the provider");
    let first_snapshot = core.subscribe(&parent).await.unwrap().snapshot;
    let second_snapshot = core.subscribe(&parent).await.unwrap().snapshot;
    assert_eq!(
        first_snapshot.seq, second_snapshot.seq,
        "subscribing must not create an unpublished sequence gap"
    );
    let child_id = first_snapshot.state.children[0].session.clone();
    let child_snapshot = core.subscribe(&child_id).await.unwrap().snapshot;
    assert_eq!(child_snapshot.parent.as_ref().unwrap().session, parent);
    assert_eq!(child_snapshot.state.agent.as_deref(), Some("review"));
    assert!(child.request.messages.iter().any(|message| matches!(
        message,
        Message::System { content, .. } if content.contains("Review the code carefully.")
    )));
    child.finish("Review complete");
    let resumed = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    resumed.finish("Parent complete");
    until(&mut subscription, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    let snapshot = core.subscribe(&parent).await.unwrap().snapshot;
    let outcome: serde_json::Value = serde_json::from_str(
        snapshot
            .blocks
            .iter()
            .find_map(|block| {
                block
                    .tool
                    .as_ref()
                    .filter(|tool| tool.name == "subagent")
                    .and_then(|tool| tool.output.as_deref())
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(outcome["status"], "completed");
    assert_eq!(outcome["response"], "Review complete");
    assert_eq!(snapshot.state.children[0].status, "completed");
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_the_child_returns_user_cancelled_to_the_parent() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let parent = core.create(Some("parent-cancel".into())).await.unwrap();
    let mut subscription = core.subscribe(&parent).await.unwrap();
    core.command(&parent, prompt("delegate")).await.unwrap();
    harness.next().await.tool(
        "subagent",
        json!({
            "agent": "assistant", "prompt": "work"
        }),
    );
    let _working_child = harness.next().await;
    let child = core
        .subscribe(&parent)
        .await
        .unwrap()
        .snapshot
        .state
        .children[0]
        .session
        .clone();
    let child_turn = core
        .subscribe(&child)
        .await
        .unwrap()
        .snapshot
        .state
        .turn_id
        .unwrap();
    core.command(
        &child,
        Action::Cancel {
            turn_id: child_turn,
        },
    )
    .await
    .unwrap();
    let continuation = harness.next().await;
    let result = continuation
        .request
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Tool { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .expect("the caller receives a tool result");
    let outcome: serde_json::Value = serde_json::from_str(result).unwrap();
    assert_eq!(outcome["status"], "user_cancelled");
    continuation.finish("I will continue");
    until(&mut subscription, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    let snapshot = core.subscribe(&parent).await.unwrap().snapshot;
    assert_eq!(snapshot.state.children[0].status, "user_cancelled");
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn steering_the_parent_reaches_the_working_child_before_parent_resumes() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let parent = core.create(Some("parent-steer".into())).await.unwrap();
    let mut subscription = core.subscribe(&parent).await.unwrap();
    core.command(&parent, prompt("delegate")).await.unwrap();
    harness.next().await.tool(
        "subagent",
        json!({
            "agent": "assistant", "prompt": "work"
        }),
    );
    let child_request = harness.next().await;
    let child = core
        .subscribe(&parent)
        .await
        .unwrap()
        .snapshot
        .state
        .children[0]
        .session
        .clone();
    let accepted = core
        .command(&parent, prompt("focus on the tests"))
        .await
        .unwrap();
    assert_eq!(accepted.routed_to.as_deref(), Some(child.as_str()));
    child_request.finish("first answer");
    let followup = harness.next().await;
    assert!(followup.request.messages.iter().any(|message| matches!(
        message, Message::Steer { content, .. } if content == "focus on the tests"
    )));
    followup.finish("revised answer");
    let resumed = harness.next().await;
    let result = resumed
        .request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Tool { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .unwrap();
    let outcome: serde_json::Value = serde_json::from_str(result).unwrap();
    assert_eq!(outcome["response"], "revised answer");
    resumed.finish("parent done");
    until(&mut subscription, |event| {
        matches!(event, Event::GenerationFinished { .. })
    })
    .await;
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_parent_stops_nested_children_without_deadlocking() {
    let mut harness = Harness::new().await;
    let core = harness.core.clone();
    let parent = core.create(Some("nested-cancel".into())).await.unwrap();
    let parent_turn = core
        .command(&parent, prompt("delegate twice"))
        .await
        .unwrap()
        .turn_id
        .unwrap();
    harness.next().await.tool(
        "subagent",
        json!({
            "agent": "assistant", "prompt": "delegate again"
        }),
    );
    harness.next().await.tool(
        "subagent",
        json!({
            "agent": "assistant", "prompt": "work"
        }),
    );
    let _grandchild = harness.next().await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.command(
            &parent,
            Action::Cancel {
                turn_id: parent_turn,
            },
        ),
    )
    .await
    .expect("nested cancellation must settle")
    .unwrap();
    let root = core.subscribe(&parent).await.unwrap().snapshot;
    assert!(root.state.turn_id.is_none());
    core.shutdown().await.unwrap();
}
