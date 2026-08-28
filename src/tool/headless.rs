use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use serde_json::{Value, json};
use tar::Archive;
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use url::Url;

const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const RUNTIME_ARCHIVE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/patchright-runtime.tar.gz"));
const RUNTIME_HASH: &str = env!("ROPE_PATCHRIGHT_RUNTIME_HASH");
const RUNTIME_TARGET: &str = env!("ROPE_PATCHRIGHT_RUNTIME_TARGET");

type Reply = std::result::Result<Value, String>;
type Waiters = Arc<StdMutex<HashMap<u64, oneshot::Sender<Reply>>>>;

pub struct HeadlessBrowser {
    executable: PathBuf,
    next_id: AtomicU64,
    state: Mutex<BrowserState>,
}

#[derive(Deserialize)]
pub struct BrowserPage {
    pub title: String,
    pub url: String,
    pub html: String,
    pub visible_text: String,
    pub blocks: Vec<BrowserBlock>,
    pub links: Vec<BrowserLink>,
}

#[derive(Deserialize)]
pub struct BrowserBlock {
    pub tag: String,
    pub text: String,
}

#[derive(Deserialize)]
pub struct BrowserLink {
    pub text: String,
    pub url: String,
}

#[derive(Default)]
struct BrowserState {
    sidecar: Option<Sidecar>,
    profile: Option<TempDir>,
}

struct Sidecar {
    child: Child,
    commands: mpsc::UnboundedSender<String>,
    waiters: Waiters,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

struct CancelGuard {
    id: u64,
    commands: mpsc::UnboundedSender<String>,
    armed: bool,
}

impl HeadlessBrowser {
    pub fn discover() -> Option<Self> {
        if !runtime_available() {
            return None;
        }
        find_browser().map(|executable| Self {
            executable,
            next_id: AtomicU64::new(1),
            state: Mutex::new(BrowserState::default()),
        })
    }

    pub async fn load(&self, url: &Url) -> Result<BrowserPage> {
        timeout(LOAD_TIMEOUT, self.load_inner(url))
            .await
            .context("browser timed out")?
    }

    async fn load_inner(&self, url: &Url) -> Result<BrowserPage> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let receiver;
        let commands;
        {
            let mut state = self.state.lock().await;
            let stopped = match state.sidecar.as_mut() {
                Some(sidecar) => sidecar.child.try_wait()?.is_some(),
                None => false,
            };
            if stopped {
                state.sidecar.take();
            }
            if state.profile.is_none() {
                state.profile = Some(
                    tempfile::Builder::new()
                        .prefix("rope-browser-")
                        .tempdir()
                        .context("create browser profile")?,
                );
            }
            if state.sidecar.is_none() {
                let runtime = tokio::task::spawn_blocking(ensure_runtime)
                    .await
                    .context("prepare Patchright runtime task")??;
                let profile = state.profile.as_ref().unwrap().path();
                state.sidecar = Some(Sidecar::start(&runtime, profile, &self.executable).await?);
            }
            let sidecar = state.sidecar.as_ref().unwrap();
            receiver = sidecar.request(id, "load", Some(url.as_str()))?;
            commands = sidecar.commands.clone();
        }

        let mut cancel = CancelGuard {
            id,
            commands,
            armed: true,
        };
        let reply = receiver.await.context("Patchright browser stopped")?;
        cancel.armed = false;
        let value = reply.map_err(anyhow::Error::msg)?;
        serde_json::from_value(value).context("decode page from Patchright")
    }

    pub async fn shutdown(&self) {
        let (sidecar, profile) = {
            let mut state = self.state.lock().await;
            (state.sidecar.take(), state.profile.take())
        };
        if let Some(sidecar) = sidecar {
            sidecar
                .shutdown(self.next_id.fetch_add(1, Ordering::Relaxed))
                .await;
        }
        drop(profile);
    }
}

pub fn browser_executable() -> Option<PathBuf> {
    find_browser()
}

pub async fn prepare_runtime() -> Result<Option<PathBuf>> {
    if !runtime_available() {
        return Ok(None);
    }
    tokio::task::spawn_blocking(ensure_runtime)
        .await
        .context("prepare Patchright runtime task")?
        .map(Some)
}

impl Sidecar {
    async fn start(runtime: &Path, profile: &Path, browser: &Path) -> Result<Self> {
        let mut command = Command::new(runtime.join(node_name()));
        command
            .arg(runtime.join("sidecar.cjs"))
            .arg(profile)
            .arg(browser)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("start Patchright browser helper")?;
        let stdin = child
            .stdin
            .take()
            .context("capture Patchright helper stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("capture Patchright helper stdout")?;
        let stderr_pipe = child
            .stderr
            .take()
            .context("capture Patchright helper stderr")?;
        let errors = Arc::new(StdMutex::new(String::new()));
        let captured = errors.clone();
        let stderr = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr_pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut output = captured.lock().expect("Patchright stderr mutex poisoned");
                if output.len() < 32 * 1024 {
                    output.push_str(&line);
                    output.push('\n');
                }
            }
        });

        let mut lines = BufReader::new(stdout).lines();
        let ready = match timeout(START_TIMEOUT, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                stderr.await.ok();
                bail!(
                    "Patchright browser helper exited during startup: {}",
                    startup_error(&errors)
                );
            }
            Ok(Err(error)) => {
                child.kill().await.ok();
                bail!("read Patchright startup reply: {error}");
            }
            Err(_) => {
                child.kill().await.ok();
                stderr.await.ok();
                bail!(
                    "Patchright browser startup timed out: {}",
                    startup_error(&errors)
                );
            }
        };
        let message: Value =
            serde_json::from_str(&ready).context("invalid Patchright startup reply")?;
        if message.get("ready").and_then(Value::as_bool) != Some(true) {
            let error = errors
                .lock()
                .expect("Patchright stderr mutex poisoned")
                .trim()
                .to_owned();
            child.kill().await.ok();
            bail!("Patchright browser failed to start: {error}");
        }

        let (commands, mut command_rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(command) = command_rx.recv().await {
                if stdin.write_all(command.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let waiters: Waiters = Arc::new(StdMutex::new(HashMap::new()));
        let pending = waiters.clone();
        let reader = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let reply = match message.get("error").and_then(Value::as_str) {
                    Some(error) => Err(error.to_owned()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some(waiter) = pending
                    .lock()
                    .expect("Patchright waiter mutex poisoned")
                    .remove(&id)
                {
                    waiter.send(reply).ok();
                }
            }
            for (_, waiter) in pending
                .lock()
                .expect("Patchright waiter mutex poisoned")
                .drain()
            {
                waiter.send(Err("Patchright browser stopped".into())).ok();
            }
        });

        Ok(Self {
            child,
            commands,
            waiters,
            reader,
            writer,
            stderr,
        })
    }

    fn request(
        &self,
        id: u64,
        method: &str,
        url: Option<&str>,
    ) -> Result<oneshot::Receiver<Reply>> {
        let (sender, receiver) = oneshot::channel();
        self.waiters
            .lock()
            .expect("Patchright waiter mutex poisoned")
            .insert(id, sender);
        let mut request = json!({ "id": id, "method": method });
        if let Some(url) = url {
            request["url"] = Value::String(url.to_owned());
        }
        if self.commands.send(request.to_string()).is_err() {
            self.waiters
                .lock()
                .expect("Patchright waiter mutex poisoned")
                .remove(&id);
            bail!("Patchright browser stopped");
        }
        Ok(receiver)
    }

    async fn shutdown(mut self, id: u64) {
        if let Ok(reply) = self.request(id, "shutdown", None) {
            let _ = timeout(SHUTDOWN_TIMEOUT, reply).await;
        }
        if timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await.is_err() {
            self.child.kill().await.ok();
        }
    }
}

fn startup_error(errors: &StdMutex<String>) -> String {
    let error = errors
        .lock()
        .expect("Patchright stderr mutex poisoned")
        .trim()
        .to_owned();
    if error.is_empty() {
        "no diagnostics".into()
    } else {
        error
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
        self.stderr.abort();
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.commands
                .send(json!({ "id": self.id, "method": "cancel" }).to_string())
                .ok();
        }
    }
}

fn runtime_available() -> bool {
    runtime_override().is_some() || !RUNTIME_ARCHIVE.is_empty()
}

fn ensure_runtime() -> Result<PathBuf> {
    if let Some(runtime) = runtime_override() {
        validate_runtime(&runtime)?;
        return Ok(runtime);
    }
    if RUNTIME_ARCHIVE.is_empty() {
        bail!(
            "Patchright runtime is not embedded; run scripts/prepare-patchright-runtime.sh before building"
        );
    }

    let cache = directories::BaseDirs::new()
        .context("find user cache directory")?
        .cache_dir()
        .join("rope/patchright");
    let runtime = cache.join(format!("{}-{RUNTIME_TARGET}", &RUNTIME_HASH[..16]));
    if runtime.exists() {
        if runtime.join(".complete").is_file() && validate_runtime(&runtime).is_ok() {
            return Ok(runtime);
        }
        fs::remove_dir_all(&runtime).context("remove incomplete Patchright runtime")?;
    }

    fs::create_dir_all(&cache).context("create Patchright cache directory")?;
    let staging = tempfile::Builder::new()
        .prefix("extract-")
        .tempdir_in(&cache)
        .context("create Patchright staging directory")?;
    Archive::new(GzDecoder::new(RUNTIME_ARCHIVE))
        .unpack(staging.path())
        .context("extract Patchright runtime")?;
    validate_runtime(staging.path())?;
    make_node_executable(staging.path())?;
    fs::write(staging.path().join(".complete"), RUNTIME_HASH)
        .context("mark Patchright runtime complete")?;
    match fs::rename(staging.path(), &runtime) {
        Ok(()) => {}
        Err(_error) if runtime.join(".complete").is_file() => {
            validate_runtime(&runtime)?;
            return Ok(runtime);
        }
        Err(error) => return Err(error).context("install Patchright runtime"),
    }
    Ok(runtime)
}

fn runtime_override() -> Option<PathBuf> {
    std::env::var_os("ROPE_PATCHRIGHT_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| validate_runtime(path).is_ok())
}

fn validate_runtime(path: &Path) -> Result<()> {
    for entry in [
        path.join(node_name()),
        path.join("sidecar.cjs"),
        path.join("node_modules/patchright-core"),
    ] {
        if !entry.exists() {
            bail!(
                "incomplete Patchright runtime: {} is missing",
                entry.display()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_node_executable(runtime: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let node = runtime.join(node_name());
    let mut permissions = fs::metadata(&node)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(node, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_node_executable(_runtime: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn node_name() -> &'static str {
    "node.exe"
}

#[cfg(not(windows))]
fn node_name() -> &'static str {
    "node"
}

fn find_browser() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ROPE_BROWSER").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }

    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "microsoft-edge",
    ];
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .flat_map(|directory| names.map(|name| directory.join(name)))
        .find(|path| path.is_file())
        .or_else(platform_browser)
}

#[cfg(target_os = "macos")]
fn platform_browser() -> Option<PathBuf> {
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

#[cfg(target_os = "windows")]
fn platform_browser() -> Option<PathBuf> {
    ["PROGRAMFILES", "PROGRAMFILES(X86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .flat_map(|root| {
            [
                "Google/Chrome/Application/chrome.exe",
                "Microsoft/Edge/Application/msedge.exe",
                "BraveSoftware/Brave-Browser/Application/brave.exe",
            ]
            .map(move |suffix| PathBuf::from(&root).join(suffix))
        })
        .find(|path| path.is_file())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_browser() -> Option<PathBuf> {
    None
}
