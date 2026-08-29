use std::{
    collections::HashMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use similar::TextDiff;
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::{Notify, mpsc, watch},
};

use super::{ExecutionPlan, PlanStatus, Tool, ToolResult};

pub struct ReadTool(pub PathBuf);
pub struct WriteTool(pub PathBuf);
pub struct EditTool(pub PathBuf);
pub struct ShellTool(pub Arc<ShellJobManager>);
pub struct ShellPollTool(pub Arc<ShellJobManager>);
pub struct ShellCancelTool(pub Arc<ShellJobManager>);
pub struct SearchFilesTool {
    root: PathBuf,
    ripgrep: bool,
}
pub struct ListFilesTool {
    root: PathBuf,
    ripgrep: bool,
}
pub struct ViewImageTool(pub PathBuf);
pub struct UpdatePlanTool;

impl SearchFilesTool {
    pub fn new(root: PathBuf, ripgrep: bool) -> Self {
        Self { root, ripgrep }
    }
}

impl ListFilesTool {
    pub fn new(root: PathBuf, ripgrep: bool) -> Self {
        Self { root, ripgrep }
    }
}

fn path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn file_diff(root: &Path, target: &Path, before: Option<&str>, after: &str) -> Option<String> {
    if before == Some(after) {
        return None;
    }
    let path = target
        .strip_prefix(root)
        .unwrap_or(target)
        .to_string_lossy();
    let old = before.map_or_else(|| "/dev/null".into(), |_| format!("a/{path}"));
    let new = format!("b/{path}");
    Some(
        TextDiff::from_lines(before.unwrap_or_default(), after)
            .unified_diff()
            .header(&old, &new)
            .to_string(),
    )
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 file"
    }
    fn schema(&self) -> Value {
        object(json!({ "path": { "type": "string" } }), &["path"])
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args: Args = serde_json::from_value(args)?;
        Ok(ToolResult {
            output: tokio::fs::read_to_string(path(&self.0, &args.path)).await?,
            image: None,
            diff: None,
        })
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write a UTF-8 file, replacing its contents"
    }
    fn schema(&self) -> Value {
        object(
            json!({ "path": { "type": "string" }, "content": { "type": "string" } }),
            &["path", "content"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let target = path(&self.0, &args.path);
        let before = match tokio::fs::read_to_string(&target).await {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let diff = file_diff(&self.0, &target, before.as_deref(), &args.content);
        tokio::fs::write(&target, args.content).await?;
        Ok(ToolResult {
            output: format!("wrote {}", target.display()),
            image: None,
            diff,
        })
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Replace one exact string in a UTF-8 file"
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "path": { "type": "string" }, "old": { "type": "string" }, "new": { "type": "string" }
            }),
            &["path", "old", "new"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            old: String,
            new: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let target = path(&self.0, &args.path);
        let content = tokio::fs::read_to_string(&target).await?;
        let matches = content.matches(&args.old).count();
        if matches != 1 {
            bail!(
                "expected one match in {}, found {matches}",
                target.display()
            );
        }
        let edited = content.replacen(&args.old, &args.new, 1);
        let diff = file_diff(&self.0, &target, Some(&content), &edited);
        tokio::fs::write(&target, edited).await?;
        Ok(ToolResult {
            output: format!("edited {}", target.display()),
            image: None,
            diff,
        })
    }
}

/// Default and cap for the model-suggested `yield_time_ms`.
const DEFAULT_SHELL_YIELD: u64 = 10_000;
const MAX_SHELL_YIELD: u64 = 30_000;

/// Terminal or in-flight state of one shell job. `None` exit codes mark
/// signal deaths, which have no numeric code; the envelope renders them as
/// `-1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellJobState {
    Running,
    Finished(Option<i32>),
    Cancelled(Option<i32>),
}

impl ShellJobState {
    fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Running => None,
            Self::Finished(code) | Self::Cancelled(code) => *code,
        }
    }
}

/// One command started through the `shell` tool that has not been fully
/// delivered to the model yet.
struct ShellJob {
    /// Combined stdout and stderr, in arrival order.
    buffer: String,
    /// Byte offset into `buffer` already returned by `shell`/`shell_poll`.
    delivered: usize,
    state: ShellJobState,
    /// Signalled on new output and on the transition to a terminal state.
    /// Arc'd so waiters can hold an owned `Notified` across the unlock.
    notify: Arc<Notify>,
    /// The worker kills the child process once this flips to true.
    cancel: watch::Sender<bool>,
    /// Background task owning the child process and its pipes.
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// Shared state behind the `shell`, `shell_poll`, and `shell_cancel` tools.
pub struct ShellJobManager {
    working_dir: PathBuf,
    inner: Mutex<ShellJobsInner>,
}

struct ShellJobsInner {
    next_id: u64,
    jobs: HashMap<String, ShellJob>,
}

impl ShellJobManager {
    pub fn new(working_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            working_dir,
            inner: Mutex::new(ShellJobsInner {
                next_id: 0,
                jobs: HashMap::new(),
            }),
        })
    }

    /// Spawns `command` in the working directory and returns its job ID.
    fn start(jobs: Arc<Self>, command: &str) -> Result<String> {
        let working_dir = &jobs.working_dir;
        let mut spawn = Command::new("sh");
        spawn
            .kill_on_drop(true)
            .arg("-c")
            .arg(command)
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Give the command its own process group so cancellation can kill
        // the whole tree, not just sh.
        #[cfg(unix)]
        unsafe {
            spawn.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = spawn.spawn().context("spawn shell command")?;
        let stdout = child.stdout.take().context("open command stdout")?;
        let stderr = child.stderr.take().context("open command stderr")?;
        let job_id = {
            let mut inner = jobs.inner.lock().unwrap();
            inner.next_id += 1;
            format!("shell-{}", inner.next_id)
        };
        // Register the job before the worker can run so it can never finish
        // into a missing entry.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        jobs.inner.lock().unwrap().jobs.insert(
            job_id.clone(),
            ShellJob {
                buffer: String::new(),
                delivered: 0,
                state: ShellJobState::Running,
                notify: Arc::new(Notify::new()),
                cancel: cancel_tx,
                handle: None,
            },
        );
        let handle = tokio::spawn(shell_job_worker(
            jobs.clone(),
            job_id.clone(),
            child,
            stdout,
            stderr,
            cancel_rx,
        ));
        {
            let mut inner = jobs.inner.lock().unwrap();
            if let Some(job) = inner.jobs.get_mut(&job_id) {
                job.handle = Some(handle);
            }
        }
        Ok(job_id)
    }

    /// Waits up to `yield_time` for the job to terminate, streaming new
    /// output to `sink` while it waits, then returns the not-yet-delivered
    /// output capped so the full envelope fits in `budget` bytes.
    async fn poll(
        &self,
        job_id: &str,
        yield_time: Duration,
        budget: usize,
        sink: &Option<mpsc::UnboundedSender<String>>,
    ) -> Result<ShellJobSnapshot> {
        let unknown = || format!("unknown shell job: {job_id}");
        {
            let inner = self.inner.lock().unwrap();
            inner.jobs.get(job_id).with_context(unknown)?;
        }
        let deadline = Instant::now() + yield_time;
        // Live streaming starts where earlier calls stopped, so the UI never
        // sees output twice.
        let mut sink_cursor = {
            let inner = self.inner.lock().unwrap();
            inner
                .jobs
                .get(job_id)
                .map(|job| job.delivered)
                .with_context(unknown)?
        };
        loop {
            let terminal = {
                let inner = self.inner.lock().unwrap();
                let job = inner.jobs.get(job_id).with_context(unknown)?;
                if job.buffer.len() > sink_cursor {
                    let fresh = job.buffer[sink_cursor..].to_string();
                    if let Some(sink) = sink {
                        sink.send(fresh).ok();
                    }
                    sink_cursor = job.buffer.len();
                }
                job.state.is_terminal()
            };
            if terminal {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let notified = {
                let inner = self.inner.lock().unwrap();
                let job = inner.jobs.get(job_id).with_context(unknown)?;
                job.notify.clone().notified_owned()
            };
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {}
            }
        }
        Ok({
            let mut inner = self.inner.lock().unwrap();
            let job = inner.jobs.get_mut(job_id).with_context(unknown)?;
            let state = job.state;
            if state.is_terminal() {
                // Terminal results always carry the full remainder, so a
                // finished or cancelled envelope never points at a job with
                // undelivered bytes. Any overflow is made explicit by the
                // runtime's truncation marker.
                let output = job.buffer[job.delivered..].to_string();
                job.delivered = job.buffer.len();
                inner.jobs.remove(job_id);
                ShellJobSnapshot {
                    header: envelope_header(job_id, state),
                    output,
                }
            } else {
                // Running results must fit the call budget; the status and
                // job_id are kept over output bytes, so the model can always
                // keep polling or cancel, even under a tiny budget.
                let mut header = String::new();
                for line in [
                    format!("status: {}\n", status_str(state)),
                    format!("job_id: {job_id}\n"),
                    "output:\n".to_owned(),
                ] {
                    if header.len() + line.len() <= budget {
                        header.push_str(&line);
                    }
                }
                let end = floor_char_boundary(
                    &job.buffer,
                    job.delivered + budget.saturating_sub(header.len()),
                );
                let output = job.buffer[job.delivered..end].to_string();
                job.delivered = end;
                ShellJobSnapshot { header, output }
            }
        })
    }

    /// Asks the job's worker to kill the child process.
    fn request_cancel(&self, job_id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let job = inner
            .jobs
            .get_mut(job_id)
            .with_context(|| format!("unknown shell job: {job_id}"))?;
        job.cancel.send_replace(true);
        Ok(())
    }

    /// Waits up to `timeout` for a job to reach a terminal state.
    async fn wait_terminal(&self, job_id: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = {
                let inner = self.inner.lock().unwrap();
                match inner.jobs.get(job_id) {
                    Some(job) if job.state.is_terminal() => return,
                    Some(job) => job.notify.clone().notified_owned(),
                    None => return,
                }
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }

    /// Kills every job. Cooperative workers are given a brief grace period
    /// to unwind; anything still stuck is aborted, which kills its child via
    /// `kill_on_drop`. Safe to call only after any in-flight tool call has
    /// been aborted, so no poller can lose a job it still needs.
    pub async fn cancel_all(&self) {
        let mut handles: Vec<_> = {
            let mut inner = self.inner.lock().unwrap();
            for job in inner.jobs.values_mut() {
                job.cancel.send_replace(true);
            }
            inner
                .jobs
                .values_mut()
                .filter_map(|job| job.handle.take())
                .collect()
        };
        for handle in &mut handles {
            let mut wait = std::pin::Pin::new(&mut *handle);
            tokio::select! {
                _ = &mut wait => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => handle.abort(),
            }
        }
        self.inner.lock().unwrap().jobs.clear();
    }

    /// Kills every job and drops all retained state.
    pub async fn shutdown(&self) {
        self.cancel_all().await;
    }

    fn append_output(&self, job_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(job) = inner.jobs.get_mut(job_id) {
                job.buffer.push_str(text);
            }
        }
        self.signal(job_id);
    }

    fn finish_job(&self, job_id: &str, state: ShellJobState, trailing: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        let Some(job) = inner.jobs.get_mut(job_id) else {
            return;
        };
        if let Some(text) = trailing {
            job.buffer.push_str(&text);
        }
        job.state = state;
        job.notify.notify_waiters();
    }

    fn signal(&self, job_id: &str) {
        let inner = self.inner.lock().unwrap();
        if let Some(job) = inner.jobs.get(job_id) {
            job.notify.notify_waiters();
        }
    }
}

/// A not-yet-delivered slice of a job plus the envelope header it is
/// returned under.
struct ShellJobSnapshot {
    header: String,
    output: String,
}

impl ShellJobSnapshot {
    /// Renders the compact `status:` / `job_id:` / `output:` envelope.
    pub fn envelope(&self) -> String {
        let mut out = self.header.clone();
        out.push_str(&self.output);
        out
    }
}

fn status_str(state: ShellJobState) -> &'static str {
    match state {
        ShellJobState::Running => "running",
        ShellJobState::Finished(_) => "finished",
        ShellJobState::Cancelled(_) => "cancelled",
    }
}

fn envelope_header(job_id: &str, state: ShellJobState) -> String {
    let mut header = format!("status: {}\n", status_str(state));
    if state.is_terminal() {
        header.push_str(&format!("exit_code: {}\n", state.exit_code().unwrap_or(-1)));
    }
    header.push_str(&format!("job_id: {job_id}\noutput:\n"));
    header
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut end = index;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn clamp_yield(millis: Option<u64>) -> Duration {
    Duration::from_millis(
        millis
            .unwrap_or(DEFAULT_SHELL_YIELD)
            .clamp(1, MAX_SHELL_YIELD),
    )
}

/// Keeps a handle to the job's process group so the worker's abort path
/// still takes the whole tree down. Disarmed once the job finishes or the
/// group was already killed deliberately.
#[cfg(unix)]
struct GroupGuard(Option<libc::pid_t>);

#[cfg(unix)]
impl GroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self(
            pid.and_then(|pid| i32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .map(|pid| pid as libc::pid_t),
        )
    }

    fn kill_group(&self) {
        if let Some(pid) = self.0 {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill_group();
    }
}

#[cfg(not(unix))]
struct GroupGuard;

#[cfg(not(unix))]
impl GroupGuard {
    fn new(_pid: Option<u32>) -> Self {
        Self
    }

    fn kill_group(&self) {}

    fn disarm(&mut self) {}
}

/// Background worker owning the child process: it decodes both pipes in
/// arrival order into the job buffer and records the terminal state.
async fn shell_job_worker(
    jobs: Arc<ShellJobManager>,
    job_id: String,
    mut child: Child,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    mut cancel_rx: watch::Receiver<bool>,
) {
    enum End {
        Exited,
        Cancelled,
        ReadError(String),
    }
    let mut stdout_leftover: Vec<u8> = Vec::new();
    let mut stderr_leftover: Vec<u8> = Vec::new();
    let mut stdout_buf = vec![0u8; 8192];
    let mut stderr_buf = vec![0u8; 8192];
    let (mut stdout_done, mut stderr_done) = (false, false);
    // Survives the worker being aborted, so a cancelled job's process tree
    // cannot outlive its cancellation even on the forced-abort path.
    let mut guard = GroupGuard::new(child.id());
    let end = loop {
        if stdout_done && stderr_done {
            break End::Exited;
        }
        tokio::select! {
            _ = cancel_rx.changed() => break End::Cancelled,
            read = stdout.read(&mut stdout_buf), if !stdout_done => match read {
                Ok(0) => stdout_done = true,
                Ok(len) => jobs.append_output(
                    &job_id,
                    &decode_utf8(&mut stdout_leftover, &stdout_buf[..len]),
                ),
                Err(error) => break End::ReadError(format!("read command stdout: {error}")),
            },
            read = stderr.read(&mut stderr_buf), if !stderr_done => match read {
                Ok(0) => stderr_done = true,
                Ok(len) => jobs.append_output(
                    &job_id,
                    &decode_utf8(&mut stderr_leftover, &stderr_buf[..len]),
                ),
                Err(error) => break End::ReadError(format!("read command stderr: {error}")),
            },
        }
    };
    // Flush any trailing partial sequence held back for a read that
    // never comes.
    for leftover in [&mut stdout_leftover, &mut stderr_leftover] {
        let text = String::from_utf8_lossy(leftover).into_owned();
        leftover.clear();
        jobs.append_output(&job_id, &text);
    }
    if matches!(end, End::Cancelled) {
        // Kill the whole process group so descendants (subshells,
        // background jobs) cannot survive the cancellation.
        guard.kill_group();
        guard.disarm();
        child.kill().await.ok();
    } else {
        // A normally finished command may have backgrounded children on
        // purpose; they are left alone.
        guard.disarm();
    }
    let status = child.wait().await;
    let (state, trailing) = match (end, status) {
        (End::ReadError(error), _) => (
            ShellJobState::Finished(None),
            Some(format!("\n[shell: {error}]\n")),
        ),
        (End::Cancelled, Ok(status)) => (ShellJobState::Cancelled(status.code()), None),
        (End::Exited, Ok(status)) => (ShellJobState::Finished(status.code()), None),
        (End::Exited | End::Cancelled, Err(error)) => (
            ShellJobState::Finished(None),
            Some(format!("\n[shell: wait failed: {error}]\n")),
        ),
    };
    jobs.finish_job(&job_id, state, trailing);
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run a shell command in the current working directory. The command keeps running in the background: this call returns once the command exits or after yield_time_ms (default 10000, max 30000). The result is a compact envelope with status, job_id, and output. While status is running, call shell_poll with the job_id until the status becomes finished or cancelled; use shell_cancel to stop a command deliberately. A finished or cancelled result includes the rest of the command output. Commands still running when the turn ends are killed together with their child processes."
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "command": { "type": "string" },
                "yield_time_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 30000,
                    "default": 10000
                }
            }),
            &["command"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        self.run_streamed(args, None, usize::MAX).await
    }
    async fn run_streamed(
        &self,
        args: Value,
        sink: Option<mpsc::UnboundedSender<String>>,
        max_output_bytes: usize,
    ) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
            #[serde(default)]
            yield_time_ms: Option<u64>,
        }
        let args: Args = serde_json::from_value(args)?;
        let job_id = ShellJobManager::start(self.0.clone(), &args.command)?;
        let snapshot = self
            .0
            .poll(
                &job_id,
                clamp_yield(args.yield_time_ms),
                max_output_bytes,
                &sink,
            )
            .await?;
        Ok(ToolResult {
            output: snapshot.envelope(),
            image: None,
            diff: None,
        })
    }
    async fn cancel_active(&self) {
        self.0.cancel_all().await;
    }
    async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}

#[async_trait]
impl Tool for ShellPollTool {
    fn name(&self) -> &str {
        "shell_poll"
    }
    fn description(&self) -> &str {
        "Retrieve additional output from a shell job started with the shell tool. Waits up to yield_time_ms (default 10000, max 30000) and returns only output not delivered by earlier calls, or returns immediately once the job reaches a terminal status. Keep polling with the same job_id until the status becomes finished or cancelled; a finished or cancelled result includes all remaining output, after which the job is gone."
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "job_id": { "type": "string" },
                "yield_time_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 30000,
                    "default": 10000
                }
            }),
            &["job_id"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        self.run_streamed(args, None, usize::MAX).await
    }
    async fn run_streamed(
        &self,
        args: Value,
        sink: Option<mpsc::UnboundedSender<String>>,
        max_output_bytes: usize,
    ) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            job_id: String,
            #[serde(default)]
            yield_time_ms: Option<u64>,
        }
        let args: Args = serde_json::from_value(args)?;
        let snapshot = self
            .0
            .poll(
                &args.job_id,
                clamp_yield(args.yield_time_ms),
                max_output_bytes,
                &sink,
            )
            .await?;
        Ok(ToolResult {
            output: snapshot.envelope(),
            image: None,
            diff: None,
        })
    }
    async fn cancel_active(&self) {
        self.0.cancel_all().await;
    }
    async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}

#[async_trait]
impl Tool for ShellCancelTool {
    fn name(&self) -> &str {
        "shell_cancel"
    }
    fn description(&self) -> &str {
        "Stop a shell job started with the shell tool. Kills the command and its child processes and returns the job's remaining output with status cancelled; a job that already finished returns its remaining output with its exit status."
    }
    fn schema(&self) -> Value {
        object(json!({ "job_id": { "type": "string" } }), &["job_id"])
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        self.run_streamed(args, None, usize::MAX).await
    }
    async fn run_streamed(
        &self,
        args: Value,
        sink: Option<mpsc::UnboundedSender<String>>,
        max_output_bytes: usize,
    ) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            job_id: String,
        }
        let args: Args = serde_json::from_value(args)?;
        self.0.request_cancel(&args.job_id)?;
        self.0
            .wait_terminal(&args.job_id, Duration::from_secs(30))
            .await;
        let snapshot = self
            .0
            .poll(
                &args.job_id,
                Duration::from_millis(1),
                max_output_bytes,
                &sink,
            )
            .await?;
        Ok(ToolResult {
            output: snapshot.envelope(),
            image: None,
            diff: None,
        })
    }
    async fn cancel_active(&self) {
        self.0.cancel_all().await;
    }
    async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}

/// Decodes the longest prefix of `leftover + chunk` that can be settled now,
/// carrying at most the trailing incomplete character (three bytes) into
/// `leftover`. Invalid bytes are emitted lossy once they can no longer join
/// a pending sequence.
fn decode_utf8(leftover: &mut Vec<u8>, chunk: &[u8]) -> String {
    leftover.extend_from_slice(chunk);
    // A valid UTF-8 sequence is at most four bytes, so only the tail can
    // still change.
    let min_take = leftover.len().saturating_sub(4);
    let take = (min_take..=leftover.len())
        .rev()
        .find(|end| std::str::from_utf8(&leftover[..*end]).is_ok())
        .unwrap_or(min_take);
    let text = String::from_utf8_lossy(&leftover[..take]).into_owned();
    leftover.drain(..take);
    text
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }
    fn description(&self) -> &str {
        "Search file contents with a ripgrep-compatible regular expression. Respects ignore files."
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "pattern": { "type": "string" }, "path": { "type": "string", "default": "." }
            }),
            &["pattern"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
            path: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let search_path = args.path.unwrap_or_else(|| ".".into());
        if self.ripgrep {
            let mut command = Command::new("rg");
            command
                .kill_on_drop(true)
                .arg("--line-number")
                .arg("--color=never")
                .arg("--no-require-git")
                .arg("--path-separator")
                .arg("/")
                .arg("--")
                .arg(&args.pattern)
                .arg(&search_path)
                .current_dir(&self.root);
            match command.output().await {
                Ok(output) => {
                    if !output.status.success() && output.status.code() != Some(1) {
                        bail!(
                            "rg exited with {}\n{}",
                            output.status,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    return Ok(ToolResult {
                        output: String::from_utf8_lossy(&output.stdout).into_owned(),
                        image: None,
                        diff: None,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("run rg"),
            }
        }

        let root = self.root.clone();
        let output = tokio::task::spawn_blocking(move || {
            search_files_fallback(&root, &search_path, &args.pattern)
        })
        .await??;
        Ok(ToolResult {
            output,
            image: None,
            diff: None,
        })
    }
}

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }
    fn description(&self) -> &str {
        "List files matching a glob pattern. Respects ignore files."
    }
    fn schema(&self) -> Value {
        object(json!({ "pattern": { "type": "string" } }), &["pattern"])
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let matcher = globset::Glob::new(&args.pattern)?.compile_matcher();
        if self.ripgrep {
            let mut command = Command::new("rg");
            command
                .kill_on_drop(true)
                .arg("--files")
                .arg("--color=never")
                .arg("--no-require-git")
                .arg("--path-separator")
                .arg("/")
                .current_dir(&self.root);
            match command.output().await {
                Ok(output) => {
                    if !output.status.success() && output.status.code() != Some(1) {
                        bail!(
                            "rg exited with {}\n{}",
                            output.status,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    let mut paths = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter(|path| matcher.is_match(path))
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    paths.sort();
                    return Ok(ToolResult {
                        output: paths.join("\n"),
                        image: None,
                        diff: None,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("run rg"),
            }
        }

        let root = self.root.clone();
        let output =
            tokio::task::spawn_blocking(move || list_files_fallback(&root, &matcher)).await??;
        Ok(ToolResult {
            output,
            image: None,
            diff: None,
        })
    }
}

fn search_files_fallback(root: &Path, search_path: &str, pattern: &str) -> Result<String> {
    let regex = regex::Regex::new(pattern)?;
    let target = path(root, search_path);
    let files = if target.is_file() {
        vec![target]
    } else {
        ignore::WalkBuilder::new(target)
            .require_git(false)
            .build()
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                    Some(Ok(entry.into_path()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut output = String::new();
    for file in files {
        let content = match std::fs::read_to_string(&file) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(error) => return Err(error.into()),
        };
        let display = normalized_path(file.strip_prefix(root).unwrap_or(&file));
        for (line, content) in content.lines().enumerate() {
            if regex.is_match(content) {
                writeln!(output, "{display}:{}:{content}", line + 1)?;
            }
        }
    }
    Ok(output)
}

fn list_files_fallback(root: &Path, matcher: &globset::GlobMatcher) -> Result<String> {
    let mut paths = ignore::WalkBuilder::new(root)
        .require_git(false)
        .build()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
                let path = normalized_path(path);
                matcher.is_match(&path).then_some(Ok(path))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    paths.sort();
    Ok(paths.join("\n"))
}

fn normalized_path(path: &Path) -> String {
    normalize_path_separator(&path.to_string_lossy(), std::path::MAIN_SEPARATOR)
}

fn normalize_path_separator(path: &str, separator: char) -> String {
    if separator == '/' {
        path.to_owned()
    } else {
        path.replace(separator, "/")
    }
}

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }
    fn description(&self) -> &str {
        "View a local image file"
    }
    fn schema(&self) -> Value {
        object(json!({ "path": { "type": "string" } }), &["path"])
    }
    fn vision_only(&self) -> bool {
        true
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let target = path(&self.0, &args.path);
        let extension = target
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mime_type = match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => bail!("unsupported image format: {}", target.display()),
        };
        let data = tokio::fs::read(&target)
            .await
            .with_context(|| format!("read image {}", target.display()))?;
        Ok(ToolResult {
            output: format!("viewed {}", target.display()),
            image: Some(crate::runtime::ImageContent {
                mime_type: mime_type.into(),
                data: STANDARD.encode(data),
                path: None,
                width: 0,
                height: 0,
            }),
            diff: None,
        })
    }
}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn description(&self) -> &str {
        "Create or replace the execution plan for substantial multi-step work. Use it for tasks with at least three dependent steps or meaningful uncertainty; skip simple answers and single edits. Send the complete plan on every update, keep steps concise and verifiable, and have at most one in_progress step."
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "explanation": {
                    "type": "string",
                    "description": "Optional concise reason why the plan changed"
                },
                "plan": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            }),
            &["plan"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        let mut plan: ExecutionPlan = serde_json::from_value(args)?;
        if plan.plan.is_empty() {
            bail!("plan must contain at least one step");
        }
        if plan
            .plan
            .iter()
            .filter(|step| step.status == PlanStatus::InProgress)
            .count()
            > 1
        {
            bail!("plan may contain at most one in_progress step");
        }
        for step in &mut plan.plan {
            step.step = step.step.trim().to_owned();
            if step.step.is_empty() {
                bail!("plan steps must not be empty");
            }
        }
        plan.explanation = plan
            .explanation
            .map(|explanation| explanation.trim().to_owned())
            .filter(|explanation| !explanation.is_empty());
        Ok(ToolResult {
            output: serde_json::to_string_pretty(&plan)?,
            image: None,
            diff: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[tokio::test]
    async fn write_and_edit_return_only_their_file_diff() {
        let root = std::env::temp_dir().join(format!(
            "rope-tool-diff-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("existing.txt"), "one\ntwo\n")
            .await
            .unwrap();

        let edit = EditTool(root.clone())
            .run(json!({
                "path": "existing.txt",
                "old": "two",
                "new": "three"
            }))
            .await
            .unwrap();
        let edit_diff = edit.diff.unwrap();
        assert!(edit_diff.contains("--- a/existing.txt"));
        assert!(edit_diff.contains("+++ b/existing.txt"));
        assert!(edit_diff.contains("-two"));
        assert!(edit_diff.contains("+three"));

        let write = WriteTool(root.clone())
            .run(json!({ "path": "new.txt", "content": "new file\n" }))
            .await
            .unwrap();
        let write_diff = write.diff.unwrap();
        assert!(write_diff.contains("--- /dev/null"));
        assert!(write_diff.contains("+++ b/new.txt"));
        assert!(write_diff.contains("+new file"));

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn file_search_fallbacks_respect_gitignore() {
        let root = std::env::temp_dir().join(format!(
            "rope-file-search-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(root.join("nested"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(root.join("ignored"))
            .await
            .unwrap();
        tokio::fs::write(root.join(".gitignore"), "ignored.rs\nignored/\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("visible.rs"), "needle\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("ignored.rs"), "needle\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("nested/match.rs"), "needle nested\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("ignored/hidden.rs"), "needle\n")
            .await
            .unwrap();

        let listed = ListFilesTool::new(root.clone(), false)
            .run(json!({ "pattern": "**/*.rs" }))
            .await
            .unwrap()
            .output;
        assert_eq!(listed, "nested/match.rs\nvisible.rs");

        let searched = SearchFilesTool::new(root.clone(), false)
            .run(json!({ "pattern": "needle" }))
            .await
            .unwrap()
            .output;
        assert!(searched.contains("visible.rs:1:needle"));
        assert!(searched.contains("nested/match.rs:1:needle nested"));
        assert!(!searched.contains("ignored"));

        if super::super::ripgrep_available() {
            let listed_with_ripgrep = ListFilesTool::new(root.clone(), true)
                .run(json!({ "pattern": "**/*.rs" }))
                .await
                .unwrap()
                .output;
            assert_eq!(listed_with_ripgrep, listed);

            let searched_with_ripgrep = SearchFilesTool::new(root.clone(), true)
                .run(json!({ "pattern": "needle" }))
                .await
                .unwrap()
                .output;
            assert!(searched_with_ripgrep.contains("visible.rs:1:needle"));
            assert!(searched_with_ripgrep.contains("nested/match.rs:1:needle nested"));
            assert!(!searched_with_ripgrep.contains("ignored"));
        }

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    /// Everything after the `output:` header line of a shell envelope.
    fn envelope_output(envelope: &str) -> &str {
        envelope.split_once("output:\n").unwrap().1
    }

    #[tokio::test]
    async fn shell_short_command_finishes_in_the_initial_call() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let result = tool.run(json!({ "command": "echo hi" })).await.unwrap();

        assert!(result.output.starts_with("status: finished\n"));
        assert!(result.output.contains("exit_code: 0\n"));
        assert!(result.output.contains("job_id: shell-1\n"));
        assert_eq!(envelope_output(&result.output), "hi\n");
    }

    #[tokio::test]
    async fn shell_long_command_returns_a_job_id() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let result = tool
            .run(json!({ "command": "sleep 0.6", "yield_time_ms": 50 }))
            .await
            .unwrap();

        assert!(result.output.starts_with("status: running\n"));
        assert!(result.output.contains("job_id: shell-1\n"));
    }

    #[tokio::test]
    async fn shell_silent_command_times_out_as_running() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let result = tool
            .run(json!({ "command": "sleep 0.5", "yield_time_ms": 50 }))
            .await
            .unwrap();

        assert!(result.output.starts_with("status: running\n"));
        assert_eq!(envelope_output(&result.output), "");
    }

    #[tokio::test]
    async fn shell_finished_envelope_reports_nonzero_exit_with_the_output() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let result = tool
            .run(json!({ "command": "echo boom; exit 3" }))
            .await
            .unwrap();

        assert!(result.output.starts_with("status: finished\n"));
        assert!(result.output.contains("exit_code: 3\n"));
        assert_eq!(envelope_output(&result.output), "boom\n");
    }

    #[tokio::test]
    async fn shell_streams_output_while_the_command_runs() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut run = Box::pin(tool.run_streamed(
            json!({ "command": "printf a; sleep 0.2; printf b; sleep 0.2; printf c >&2" }),
            Some(tx),
            usize::MAX,
        ));
        let mut streamed = String::new();
        let (result, streamed_before_finish) = loop {
            tokio::select! {
                result = &mut run => break (result, streamed.len()),
                Some(delta) = rx.recv() => streamed.push_str(&delta),
            }
        };
        let result = result.unwrap();
        while let Ok(delta) = rx.try_recv() {
            streamed.push_str(&delta);
        }

        assert!(
            streamed_before_finish > 0,
            "expected output while the command was still running"
        );
        assert_eq!(streamed, "abc");
        assert_eq!(
            result.output,
            "status: finished\nexit_code: 0\njob_id: shell-1\noutput:\nabc"
        );
    }

    #[tokio::test]
    async fn shell_polls_return_output_exactly_once_and_in_order() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let poll = ShellPollTool(tool.0.clone());
        let first = tool
            .run(json!({
                "command": "printf a; sleep 0.2; printf b; sleep 0.2; printf c",
                "yield_time_ms": 100
            }))
            .await
            .unwrap();
        assert!(first.output.starts_with("status: running\n"));
        let mut delivered = envelope_output(&first.output).to_string();
        assert!(
            "abc".starts_with(&delivered),
            "first call delivered out-of-order or duplicated output"
        );
        let mut polls = 0;
        loop {
            polls += 1;
            let next = poll
                .run(json!({ "job_id": "shell-1", "yield_time_ms": 100 }))
                .await
                .unwrap();
            let chunk = envelope_output(&next.output);
            delivered.push_str(chunk);
            assert!(
                "abc".starts_with(&delivered),
                "poll output must continue in order without gaps or repeats: {delivered:?}"
            );
            if next.output.starts_with("status: finished\n") {
                break;
            }
            assert!(polls < 20, "command should have finished");
        }
        assert!(polls >= 1, "expected at least one poll");
        assert_eq!(delivered, "abc");
        // The fully delivered terminal job is removed.
        let gone = poll.run(json!({ "job_id": "shell-1" })).await.unwrap_err();
        assert!(gone.to_string().contains("unknown shell job"));
    }

    #[tokio::test]
    async fn shell_cancel_stops_the_job_and_returns_its_state() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let cancel = ShellCancelTool(tool.0.clone());
        let first = tool
            .run(json!({ "command": "sleep 5", "yield_time_ms": 50 }))
            .await
            .unwrap();
        assert!(first.output.starts_with("status: running\n"));

        let cancelled = cancel.run(json!({ "job_id": "shell-1" })).await.unwrap();
        assert!(cancelled.output.starts_with("status: cancelled\n"));
        assert!(cancelled.output.contains("job_id: shell-1\n"));

        // The job was delivered and removed by the cancel call.
        let gone = ShellPollTool(tool.0.clone())
            .run(json!({ "job_id": "shell-1" }))
            .await
            .unwrap_err();
        assert!(gone.to_string().contains("unknown shell job"));
    }

    #[tokio::test]
    async fn shell_utf8_split_across_reads_and_polls() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let poll = ShellPollTool(tool.0.clone());
        let first = tool
            .run(json!({
                "command": r#"printf 'h\303'; sleep 0.2; printf '\251llo'"#,
                "yield_time_ms": 60
            }))
            .await
            .unwrap();
        assert!(first.output.starts_with("status: running\n"));
        assert_eq!(envelope_output(&first.output), "h");

        let next = poll
            .run(json!({ "job_id": "shell-1", "yield_time_ms": 300 }))
            .await
            .unwrap();
        assert!(next.output.starts_with("status: finished\n"));
        assert_eq!(envelope_output(&next.output), "\u{e9}llo");
    }

    #[tokio::test]
    async fn shell_output_larger_than_one_poll_budget_continues_without_gaps() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let poll = ShellPollTool(tool.0.clone());
        let first = tool
            .run_streamed(
                json!({
                    "command": "head -c 100 /dev/zero | tr '\\0' x; sleep 0.3",
                    "yield_time_ms": 50
                }),
                None,
                100,
            )
            .await
            .unwrap();
        assert!(first.output.starts_with("status: running\n"));
        assert!(
            first.output.len() <= 100,
            "envelope must fit the poll budget"
        );
        let mut delivered = envelope_output(&first.output).to_string();
        assert!(!delivered.is_empty() && delivered.len() < 100);

        let rest = poll
            .run_streamed(json!({ "job_id": "shell-1" }), None, usize::MAX)
            .await
            .unwrap();
        assert!(rest.output.starts_with("status: finished\n"));
        delivered.push_str(envelope_output(&rest.output));
        assert_eq!(delivered, "x".repeat(100));
    }

    #[tokio::test]
    async fn shell_terminal_result_carries_all_remaining_output() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let poll = ShellPollTool(tool.0.clone());
        let result = tool
            .run_streamed(
                json!({
                    "command": "head -c 200 /dev/zero | tr '\\0' x",
                    "yield_time_ms": 500
                }),
                None,
                100,
            )
            .await
            .unwrap();
        assert!(result.output.starts_with("status: finished\n"));
        // The terminal envelope ignores the call budget: nothing is left
        // behind for a poll that would never come.
        assert_eq!(envelope_output(&result.output), "x".repeat(200));
        let gone = poll.run(json!({ "job_id": "shell-1" })).await.unwrap_err();
        assert!(gone.to_string().contains("unknown shell job"));
    }

    #[tokio::test]
    async fn shell_running_envelope_keeps_the_job_id_under_a_tight_budget() {
        let tool = ShellTool(ShellJobManager::new(std::env::temp_dir()));
        let poll = ShellPollTool(tool.0.clone());
        let first = tool
            .run_streamed(
                json!({ "command": "printf hello; sleep 0.5", "yield_time_ms": 50 }),
                None,
                40,
            )
            .await
            .unwrap();
        assert!(first.output.starts_with("status: running\n"));
        assert!(
            first.output.len() <= 40,
            "envelope must fit its budget: {:?}",
            first.output
        );
        assert!(
            first.output.contains("job_id: shell-1"),
            "the job_id must survive a tight budget: {:?}",
            first.output
        );
        assert_eq!(envelope_output(&first.output), "");

        let rest = poll
            .run_streamed(json!({ "job_id": "shell-1" }), None, usize::MAX)
            .await
            .unwrap();
        assert!(rest.output.starts_with("status: finished\n"));
        assert_eq!(envelope_output(&rest.output), "hello");
    }

    #[tokio::test]
    async fn shell_cancel_kills_descendant_processes() {
        let root = std::env::temp_dir().join(format!(
            "rope-shell-descendants-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let marker = root.join("survived");

        let tool = ShellTool(ShellJobManager::new(root.clone()));
        let cancel = ShellCancelTool(tool.0.clone());
        let first = tool
            .run(json!({
                // The delayed side effect lives in a backgrounded
                // descendant; `wait` keeps sh itself alive so the job is
                // still running when it is cancelled.
                "command": format!(
                    "(sleep 2 && echo survived > {}) & wait",
                    marker.display()
                ),
                "yield_time_ms": 50
            }))
            .await
            .unwrap();
        assert!(first.output.starts_with("status: running\n"));

        let cancelled = cancel.run(json!({ "job_id": "shell-1" })).await.unwrap();
        assert!(cancelled.output.starts_with("status: cancelled\n"));

        // If the descendant `sleep` survived the kill, it would complete the
        // command and create the marker shortly after.
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(!marker.exists(), "a descendant outlived the cancellation");
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn shell_poll_streams_live_output_while_waiting() {
        let manager = ShellJobManager::new(std::env::temp_dir());
        let tool = ShellTool(manager.clone());
        let poll = ShellPollTool(manager);
        let first = tool
            .run(json!({ "command": "sleep 0.15; printf live", "yield_time_ms": 50 }))
            .await
            .unwrap();
        assert!(first.output.starts_with("status: running\n"));
        assert_eq!(envelope_output(&first.output), "");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut run = Box::pin(poll.run_streamed(
            json!({ "job_id": "shell-1", "yield_time_ms": 400 }),
            Some(tx),
            usize::MAX,
        ));
        let mut streamed = String::new();
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                Some(delta) = rx.recv() => streamed.push_str(&delta),
            }
        }
        .unwrap();
        // The poll may send its final delta just before it completes, in
        // which case select picked the result first. The sink is dropped by
        // now, so draining the queue is complete.
        while let Ok(delta) = rx.try_recv() {
            streamed.push_str(&delta);
        }
        assert_eq!(streamed, "live");
        assert_eq!(envelope_output(&result.output), "live");
    }

    #[test]
    fn utf8_decoder_carries_partial_characters_across_reads() {
        let mut leftover: Vec<u8> = Vec::new();
        let bytes = "héllo".as_bytes();

        assert_eq!(decode_utf8(&mut leftover, &bytes[..2]), "h");
        assert_eq!(leftover, vec![0xC3]);
        assert_eq!(decode_utf8(&mut leftover, &bytes[2..]), "éllo");
        assert!(leftover.is_empty());
    }

    #[test]
    fn utf8_decoder_emits_invalid_bytes_lossy_once_settled() {
        let mut leftover: Vec<u8> = Vec::new();

        assert_eq!(decode_utf8(&mut leftover, &[0xFF, 0x61]), "");
        assert_eq!(
            decode_utf8(&mut leftover, &[0x62, 0x63, 0x64, 0x65]),
            "\u{FFFD}a"
        );
        assert_eq!(decode_utf8(&mut leftover, &[0x66]), "bcdef");
        assert!(leftover.is_empty());
    }

    #[test]
    fn tool_paths_use_portable_separators() {
        assert_eq!(
            normalize_path_separator("nested/match.rs", '/'),
            "nested/match.rs"
        );
        assert_eq!(
            normalize_path_separator(r"nested\match.rs", '\\'),
            "nested/match.rs"
        );
    }
}
