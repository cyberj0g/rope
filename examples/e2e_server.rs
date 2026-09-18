//! Headless e2e server: the real `Core` and `Server` backed by a scripted
//! model provider, so the web UI can be exercised end to end without an API
//! key. Run with `cargo run --example e2e_server`; the token and address are
//! printed to stdout and overridable via `ROPE_E2E_PORT` and `ROPE_E2E_TOKEN`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::stream;
use rope::{
    config::{Config, ModelConfig, ToolPolicies},
    core::Core,
    provider::{Provider, ResponseDelta, ResponseStream, Usage},
    runtime::CompletionRequest,
    server::Server,
    tool::Approval,
};
use std::{
    collections::VecDeque,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

const DELTA_DELAY: Duration = Duration::from_millis(30);

struct ScriptedProvider {
    root: std::path::PathBuf,
    scripts: Mutex<VecDeque<Vec<ResponseDelta>>>,
}

impl ScriptedProvider {
    fn new(root: std::path::PathBuf, scripts: Vec<Vec<ResponseDelta>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            root,
        }
    }

    fn script(&self) -> Vec<ResponseDelta> {
        match self.scripts.lock().unwrap().pop_front() {
            Some(script) => script,
            // The scripted conversation is position-based and finite; anything
            // after it gets the catch-all. It is deliberately long so the
            // regression scenarios have a stable running window to exercise
            // steering, cancellation, and the composer controls mid-turn.
            None => catchall(),
        }
    }

    /// The scripted conversation is position-based; title requests and image
    /// turns are detected from the request so they never consume a script.
    fn next(&self, request: &CompletionRequest) -> Vec<ResponseDelta> {
        use rope::runtime::Message;
        if let Some(Message::System { content, .. }) = request.messages.first() {
            if content.contains("2-3 word title") {
                eprintln!("[script] title request (no consume)");
                return text_only("Scripted e2e session");
            }
        }
        // send_file regression: a marked prompt starts the turn, and the two
        // tool results that follow each get a scripted reply — none of it
        // consumes the script queue.
        if let Some(Message::Tool {
            file: Some(file), ..
        }) = request.messages.last()
        {
            if file.path.ends_with("report.png") {
                eprintln!("[script] image sent; asking for the report");
                let path = self.root.join("notes.md").display().to_string();
                return vec![
                    ResponseDelta::ToolCall {
                        index: 0,
                        id: Some("call-send-report".into()),
                        name: Some("send_file".into()),
                        arguments: serde_json::json!({ "path": path }).to_string(),
                    },
                    ResponseDelta::Completed,
                ];
            }
            if file.path.ends_with("notes.md") {
                eprintln!("[script] report sent; wrapping up");
                return text_only(
                    "Both files are in your chat now — the picture inline, the report as a file tile.",
                );
            }
        }
        let send_file_turn = matches!(
            request.messages.iter().rev().find(|m| matches!(m, Message::User { .. } | Message::Steer { .. })),
            Some(Message::User { content, .. } | Message::Steer { content, .. })
                if content.contains("rope-e2e-send-file")
        );
        if send_file_turn {
            eprintln!("[script] send_file turn start");
            let path = self.root.join("report.png").display().to_string();
            return vec![
                ResponseDelta::Text("Here you go.".into()),
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call-send-image".into()),
                    name: Some("send_file".into()),
                    arguments: serde_json::json!({ "path": path }).to_string(),
                },
                ResponseDelta::Completed,
            ];
        }
        let has_image = matches!(
            request.messages.iter().rev().find(|m| matches!(m, Message::User { .. } | Message::Steer { .. })),
            Some(Message::User { images, .. } | Message::Steer { images, .. }) if !images.is_empty()
        );
        if has_image {
            eprintln!("[script] image turn (no consume)");
            return text_only(
                "I can see the image you attached — it came through end to end, and I'd frame the logo with a little more padding.",
            );
        }
        let rest = self.scripts.lock().unwrap().len();
        let script = self.script();
        if rest == 0 {
            eprintln!("[script] catch-all turn (queue exhausted)");
        } else {
            eprintln!("[script] consumed a scripted turn; {rest} left in queue");
        }
        script
    }
}

fn text_only(text: &str) -> Vec<ResponseDelta> {
    vec![
        ResponseDelta::Text(text.to_owned()),
        ResponseDelta::Completed,
    ]
}

fn streamed(text: &str) -> Vec<ResponseDelta> {
    let mut deltas = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(10) {
        deltas.push(ResponseDelta::Text(chunk.iter().collect()));
    }
    deltas
}

/// The long-running reply handed out for every turn once the scripted
/// conversation is exhausted. About 3 seconds of streaming at DELTA_DELAY,
/// which gives the mid-turn regression scenarios a stable window.
fn catchall() -> Vec<ResponseDelta> {
    let mut deltas = streamed(
        "That's the scripted conversation. Anything else I'd answer with the same rendering, \
         tools, and plans you just saw. I'm keeping this reply deliberately long on purpose \
         because the e2e scenarios lean on a stable running window to exercise steering, \
         cancellation, and the composer controls mid-turn. So consider this a steady, \
         unhurried stream of words that stays in flight long enough for a steering message \
         to be queued, for the cancel button to show up, and for an Escape from the focused \
         composer to interrupt the turn cleanly. There is nothing remarkable in the prose \
         itself; its only job is to hold the turn open while the driver pokes at the UI. \
         By the time you reach this sentence the turn should still be generating, the phase \
         should still read as active, and the turn id should still be present on the session \
         state, which is exactly the state the regression checks are sampling for. If you are \
         reading the tail end of this, then the window held as intended and the scenario can \
         move on with confidence that the running turn behaved the way the driver expected.",
    );
    deltas.push(ResponseDelta::Usage(Usage {
        prompt_tokens: 1102,
        total_tokens: 1188,
    }));
    deltas.push(ResponseDelta::Completed);
    deltas
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn request_body(&self, mut request: CompletionRequest) -> Result<Option<serde_json::Value>> {
        request.provider = "default".into();
        rope::provider::openai::OpenAiProvider::new(String::new(), String::new())
            .request_body(request)
    }

    async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream> {
        let deltas = self.next(&request);
        Ok(Box::pin(stream::unfold(
            std::collections::VecDeque::from(deltas),
            |mut queue| async move {
                let delta = queue.pop_front()?;
                tokio::time::sleep(DELTA_DELAY).await;
                Some((Ok(delta), queue))
            },
        )))
    }
}

fn scripts() -> Vec<Vec<ResponseDelta>> {
    let script = |parts: Vec<Vec<ResponseDelta>>| {
        let mut all = Vec::new();
        for part in parts {
            all.extend(part);
        }
        all
    };
    vec![
        // 1. Markdown showcase: reasoning, headings, lists, table, code, link.
        script(vec![
            vec![ResponseDelta::Reasoning("The user asked for a showcase. I'll cover every formatting feature in one reply so the UI can be checked at a glance.".into())],
            streamed("# Rope web UI showcase\n\nEverything below should render: **bold**, *italics*, `inline code`, and a [link](https://example.com).\n\n## Checklist\n\n- [x] streaming text\n- [ ] approvals\n- [ ] plans\n\n| Tool | Status |\n| --- | --- |\n| read | allowed |\n| shell | asks |\n\n```rust\nfn main() {\n    // greet the world\n    let message = \"hello from rope\";\n    println!(\"{message}\");\n}\n```\n\n> Ship the small thing, then make it fast."),
            vec![ResponseDelta::Usage(Usage { prompt_tokens: 412, total_tokens: 634 }), ResponseDelta::Completed],
        ]),
        // 2. Tool turn: read the notes, then summarize.
        script(vec![
            streamed("Let me read that file first."),
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call-read-1".into()),
                    name: Some("read".into()),
                    arguments: r#"{"path":"notes.md"}"#.into(),
                },
                ResponseDelta::Completed,
            ],
        ]),
        script(vec![
            streamed("The notes list three ideas, and the last one — *ship the web UI* — is exactly what we're validating right now."),
            vec![ResponseDelta::Usage(Usage { prompt_tokens: 701, total_tokens: 812 }), ResponseDelta::Completed],
        ]),
        // 3. Shell turn that requires approval.
        script(vec![
            streamed("I'll run a quick command to prove the approval flow works."),
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call-shell-1".into()),
                    name: Some("shell".into()),
                    arguments: r#"{"command":"echo 'hello from rope'"}"#.into(),
                },
                ResponseDelta::Completed,
            ],
        ]),
        script(vec![
            streamed("The command printed `hello from rope` — approvals, shell tools, and live output are all wired up."),
            vec![ResponseDelta::Usage(Usage { prompt_tokens: 918, total_tokens: 1042 }), ResponseDelta::Completed],
        ]),
        // 4. Plan turn.
        script(vec![
            streamed("Here's the plan I'm working against."),
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call-plan-1".into()),
                    name: Some("update_plan".into()),
                    arguments: r#"{"explanation":"Validation pass","plan":[{"step":"Build the mobile-first web UI","status":"completed"},{"step":"Wire image upload and camera capture","status":"in_progress"},{"step":"Run headless e2e validation","status":"pending"}]}"#.into(),
                },
                ResponseDelta::Completed,
            ],
        ]),
        script(vec![
            streamed("Plan is on the board — the image step is in progress, e2e validation is next."),
            vec![ResponseDelta::Completed],
        ]),
        // 7. Steer scenario: a long first request followed by a read tool call,
        //    so a steer sent mid-turn is injected into the follow-up request
        //    and rendered as a distinct Steer message.
        script(vec![
            streamed("Working on it now. This is a longer preamble so the turn stays open while the tool call is prepared, which is exactly when a steering message should be able to join the conversation before the next model request."),
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call-read-2".into()),
                    name: Some("read".into()),
                    arguments: r#"{"path":"notes.md"}"#.into(),
                },
                ResponseDelta::Completed,
            ],
        ]),
        script(vec![
            streamed("Done — and I noted the steering message that arrived while I was working. The notes still land on the same three ideas."),
            vec![ResponseDelta::Usage(Usage { prompt_tokens: 1310, total_tokens: 1402 }), ResponseDelta::Completed],
        ]),
    ]
}

fn git(root: &std::path::Path, args: &[&str]) {
    let status = Command::new("git").args(args).current_dir(root).output();
    assert!(
        status.as_ref().map(|o| o.status.success()).unwrap_or(false),
        "git {args:?} failed"
    );
}

fn build_project(root: &std::path::Path) -> Result<()> {
    std::fs::write(
        root.join("notes.md"),
        "# Notes\n\n- rope: a rope for coding\n- make it mobile\n- keep it small\n",
    )?;
    // A small PNG for the send_file inline-image regression.
    std::fs::write(
        root.join("report.png"),
        STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
            .context("decode e2e png")?,
    )?;
    std::fs::create_dir(root.join("src"))?;
    std::fs::write(
        root.join("src/tool.rs"),
        "pub fn tool() -> &'static str {\n    \"rope\"\n}\n",
    )?;
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "e2e@rope.local"]);
    git(root, &["config", "user.name", "Rope E2E"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial"]);
    // Leave the tree dirty so the Git pane has something to show.
    std::fs::write(
        root.join("notes.md"),
        "# Notes\n\n- rope: a rope for coding\n- make it mobile\n- keep it small\n- ship the web UI\n",
    )?;
    std::fs::write(
        root.join("web.md"),
        "New untracked file for the e2e scenario.\n",
    )?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let port: u16 = std::env::var("ROPE_E2E_PORT")
        .unwrap_or_else(|_| "8791".into())
        .parse()?;
    let token = std::env::var("ROPE_E2E_TOKEN").unwrap_or_else(|_| "e2e-token".into());
    let project = tempfile::tempdir().context("temp project")?;
    build_project(project.path())?;
    let storage = tempfile::tempdir().context("temp storage")?;

    let mut config = Config::default();
    config.model = "mock-vision".into();
    config.models = vec![
        ModelConfig {
            name: "mock-vision".into(),
            provider: "mock".into(),
            id: "mock-vision-1".into(),
            max_context_tokens: 32_768,
            temperature: Some(1.0),
            reasoning_effort: Some(rope::runtime::ReasoningEffort::Medium),
            reasoning_efforts: vec![
                rope::runtime::ReasoningEffort::Low,
                rope::runtime::ReasoningEffort::Medium,
            ],
            price_per_token: Some(0.000002),
            vision: true,
        },
        ModelConfig {
            name: "mock-mini".into(),
            provider: "mock".into(),
            id: "mock-mini-1".into(),
            max_context_tokens: 16_384,
            temperature: None,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            price_per_token: None,
            vision: false,
        },
    ];
    config.tools = ToolPolicies {
        read: Approval::Allow,
        write: Approval::Ask,
        edit: Approval::Ask,
        shell: Approval::Ask,
        search_files: Approval::Allow,
        list_files: Approval::Allow,
        org_outline: Approval::Allow,
        send_file: Approval::Allow,
        web_browser: Approval::Ask,
        web_search: Approval::Ask,
        external: Approval::Ask,
    };

    let core = Core::new(
        config,
        project.path().into(),
        storage.path().into(),
        Arc::new(ScriptedProvider::new(
            project.path().to_path_buf(),
            scripts(),
        )),
    )
    .await?;

    let server = Server::start(
        core.clone(),
        format!("127.0.0.1:{port}").parse()?,
        token.clone(),
        Vec::new(),
    )
    .await?;
    println!("e2e server on http://{}", server.address);
    println!("token: {token}");
    println!("project: {}", project.path().display());
    println!("ready");
    tokio::signal::ctrl_c().await?;
    server.stop();
    let _ = core.shutdown().await;
    Ok(())
}
