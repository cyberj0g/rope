use std::{
    collections::HashSet,
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpStream,
    process::Command,
    time::timeout,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
static PROFILE_ID: AtomicU64 = AtomicU64::new(0);

type CdpSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct HeadlessBrowser {
    executable: PathBuf,
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

#[derive(Deserialize)]
struct DebugTarget {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: Option<String>,
}

impl HeadlessBrowser {
    pub fn discover() -> Option<Self> {
        find_browser().map(|executable| Self { executable })
    }

    pub async fn load(&self, url: &Url) -> Result<BrowserPage> {
        timeout(LOAD_TIMEOUT, self.load_inner(url))
            .await
            .context("browser timed out")?
    }

    async fn load_inner(&self, url: &Url) -> Result<BrowserPage> {
        let profile = BrowserProfile::new()?;
        let mut command = Command::new(&self.executable);
        command
            .kill_on_drop(true)
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-extensions")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--remote-debugging-port=0")
            .arg("--remote-allow-origins=*")
            .arg("--window-size=1280,900")
            .arg(format!("--user-agent={USER_AGENT}"))
            .arg(format!("--user-data-dir={}", profile.0.display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = command.spawn().context("start browser")?;
        let stderr = child.stderr.take().context("capture browser stderr")?;
        let mut stderr = BufReader::new(stderr).lines();
        let browser_socket = loop {
            let line = stderr
                .next_line()
                .await?
                .context("browser exited before opening its debug port")?;
            if let Some(socket) = line
                .split_once("DevTools listening on ")
                .map(|(_, url)| url)
            {
                break Url::parse(socket)?;
            }
        };
        let port = browser_socket.port().context("debug socket has no port")?;
        let targets: Vec<DebugTarget> = reqwest::get(format!("http://127.0.0.1:{port}/json/list"))
            .await?
            .error_for_status()?
            .json()
            .await?;
        let page_socket = targets
            .into_iter()
            .find(|target| target.kind == "page")
            .and_then(|target| target.websocket_url)
            .context("browser created no page target")?;
        let (mut socket, _) = connect_async(page_socket).await?;

        cdp_send(&mut socket, 1, "Page.enable", json!({})).await?;
        cdp_send(
            &mut socket,
            2,
            "Page.setLifecycleEventsEnabled",
            json!({ "enabled": true }),
        )
        .await?;
        wait_for_responses(&mut socket, [1, 2]).await?;
        cdp_send(
            &mut socket,
            3,
            "Page.navigate",
            json!({ "url": url.as_str() }),
        )
        .await?;
        wait_for_navigation(&mut socket).await?;
        cdp_send(
            &mut socket,
            4,
            "Runtime.evaluate",
            json!({
                "expression": EXTRACTION_SCRIPT,
                "awaitPromise": true,
                "returnByValue": true,
            }),
        )
        .await?;
        let value = wait_for_response(&mut socket, 4).await?;
        let value = value
            .pointer("/result/result/value")
            .cloned()
            .context("browser returned no page content")?;
        let page = serde_json::from_value(value)?;
        child.kill().await.ok();
        Ok(page)
    }
}

async fn cdp_send(socket: &mut CdpSocket, id: u64, method: &str, params: Value) -> Result<()> {
    socket
        .send(Message::Text(
            json!({ "id": id, "method": method, "params": params })
                .to_string()
                .into(),
        ))
        .await?;
    Ok(())
}

async fn cdp_message(socket: &mut CdpSocket) -> Result<Value> {
    loop {
        let message = socket
            .next()
            .await
            .context("browser debug socket closed")??;
        if let Message::Text(text) = message {
            return Ok(serde_json::from_str(&text)?);
        }
    }
}

async fn wait_for_responses(socket: &mut CdpSocket, ids: [u64; 2]) -> Result<()> {
    let mut waiting = HashSet::from(ids);
    while !waiting.is_empty() {
        let message = cdp_message(socket).await?;
        if let Some(id) = message["id"].as_u64()
            && waiting.remove(&id)
            && let Some(error) = message.get("error")
        {
            bail!("browser command failed: {error}");
        }
    }
    Ok(())
}

async fn wait_for_navigation(socket: &mut CdpSocket) -> Result<()> {
    let mut loader = None;
    let mut loaded = HashSet::new();
    loop {
        let message = cdp_message(socket).await?;
        if message["id"] == 3 {
            if let Some(error) = message.get("error") {
                bail!("browser navigation failed: {error}");
            }
            if let Some(error) = message.pointer("/result/errorText").and_then(Value::as_str) {
                bail!("browser navigation failed: {error}");
            }
            loader = message
                .pointer("/result/loaderId")
                .and_then(Value::as_str)
                .map(str::to_owned);
        } else if message["method"] == "Page.lifecycleEvent"
            && message.pointer("/params/name").and_then(Value::as_str) == Some("load")
            && let Some(loader) = message.pointer("/params/loaderId").and_then(Value::as_str)
        {
            loaded.insert(loader.to_owned());
        }
        if loader
            .as_ref()
            .is_some_and(|loader| loaded.contains(loader))
        {
            return Ok(());
        }
    }
}

async fn wait_for_response(socket: &mut CdpSocket, id: u64) -> Result<Value> {
    loop {
        let message = cdp_message(socket).await?;
        if message["id"] == id {
            if let Some(error) = message.get("error") {
                bail!("browser command failed: {error}");
            }
            if let Some(error) = message.pointer("/result/exceptionDetails") {
                bail!("browser script failed: {error}");
            }
            return Ok(message);
        }
    }
}

const EXTRACTION_SCRIPT: &str = r#"
new Promise(resolve => setTimeout(() => {
  const root = document.querySelector('main')
    || document.querySelector('article')
    || document.querySelector('[role="main"]')
    || document.body;
  const excluded = 'script, style, noscript, svg, nav, aside, footer, [hidden], [aria-hidden="true"]';
  const containers = 'li, blockquote, pre, td, th, tr';
  const text = element => element.innerText.replace(/\s+/g, ' ').trim();
  const visible = element => {
    if (!element || element.closest(excluded) || !text(element)) return false;
    const style = getComputedStyle(element);
    return style.display !== 'none'
      && style.visibility !== 'hidden'
      && style.visibility !== 'collapse'
      && Number(style.opacity) !== 0;
  };
  const blocks = [...root.querySelectorAll('h1, h2, h3, h4, h5, h6, p, pre, li, blockquote, dt, dd, tr')]
    .filter(element => visible(element) && !element.parentElement.closest(containers))
    .map(element => ({
      tag: element.tagName.toLowerCase(),
      text: element.tagName === 'TR'
        ? [...element.querySelectorAll(':scope > th, :scope > td')].map(text).join(' | ')
        : text(element)
    }));
  const links = [...root.querySelectorAll('a[href]')]
    .filter(visible)
    .map(element => ({ text: text(element), url: element.href }));
  resolve({
    title: document.title,
    url: location.href,
    html: document.documentElement.outerHTML,
    visible_text: root.innerText,
    blocks,
    links
  });
}, 750))
"#;

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

struct BrowserProfile(PathBuf);

impl BrowserProfile {
    fn new() -> Result<Self> {
        let id = PROFILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rope-browser-{}-{id}", std::process::id()));
        std::fs::create_dir(&path).context("create temporary browser profile")?;
        Ok(Self(path))
    }
}

impl Drop for BrowserProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
