use std::{collections::HashSet, sync::Arc};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use super::{
    Tool, ToolResult,
    headless::{BrowserBlock, BrowserPage, HeadlessBrowser},
};

const MAX_LINKS: usize = 50;

pub struct WebBrowserTool {
    browser: Arc<HeadlessBrowser>,
}

#[derive(Serialize)]
struct PageLink {
    text: String,
    url: String,
}

struct PageContent {
    content: String,
    links: Vec<PageLink>,
}

impl WebBrowserTool {
    pub fn new(browser: Arc<HeadlessBrowser>) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl Tool for WebBrowserTool {
    fn name(&self) -> &str {
        "web_browser"
    }

    fn description(&self) -> &str {
        "Open an HTTP(S) URL and return visible page text and links; the result is ejected from later context after the final response"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "HTTP(S) URL to open" }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            url: String,
        }

        let args: Args = serde_json::from_value(args)?;
        let url = Url::parse(args.url.trim())?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("web_browser only supports HTTP(S) URLs");
        }

        let page = self.browser.load(&url).await?;
        let extracted = extract_page(&page);
        Ok(ToolResult {
            output: serde_json::to_string_pretty(&json!({
                "url": page.url,
                "title": page.title,
                "content": extracted.content,
                "links": extracted.links,
            }))?,
            image: None,
            diff: None,
        })
    }

    async fn shutdown(&self) {
        self.browser.shutdown().await;
    }
}

fn extract_page(page: &BrowserPage) -> PageContent {
    let blocks = page
        .blocks
        .iter()
        .filter_map(format_block)
        .collect::<Vec<_>>();
    let content = if blocks.is_empty() {
        page.visible_text.trim().to_owned()
    } else {
        blocks.join("\n\n")
    };

    let mut seen = HashSet::new();
    let links = page
        .links
        .iter()
        .filter_map(|link| {
            let mut url = Url::parse(&link.url).ok()?;
            if !matches!(url.scheme(), "http" | "https") {
                return None;
            }
            url.set_fragment(None);
            let text = link.text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() || !seen.insert(url.clone()) {
                return None;
            }
            Some(PageLink {
                text,
                url: url.into(),
            })
        })
        .take(MAX_LINKS)
        .collect();

    PageContent { content, links }
}

fn format_block(block: &BrowserBlock) -> Option<String> {
    let text = block.text.trim();
    if text.is_empty() {
        return None;
    }
    Some(match block.tag.as_str() {
        "h1" => format!("# {text}"),
        "h2" => format!("## {text}"),
        "h3" => format!("### {text}"),
        "h4" | "h5" | "h6" => format!("#### {text}"),
        "li" => format!("- {text}"),
        "blockquote" => format!("> {text}"),
        "pre" => format!("```\n{text}\n```"),
        "tr" => text.to_owned(),
        _ => text.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::headless::BrowserLink;

    fn page() -> BrowserPage {
        BrowserPage {
            title: "Example page".into(),
            url: "https://example.com/page".into(),
            html: String::new(),
            visible_text: "Fallback visible text".into(),
            blocks: vec![
                BrowserBlock {
                    tag: "h1".into(),
                    text: "Guide".into(),
                },
                BrowserBlock {
                    tag: "p".into(),
                    text: "Useful content.".into(),
                },
                BrowserBlock {
                    tag: "li".into(),
                    text: "First item".into(),
                },
            ],
            links: vec![BrowserLink {
                text: "Read docs".into(),
                url: "https://example.com/docs#start".into(),
            }],
        }
    }

    #[test]
    fn formats_browser_visible_blocks_and_links() {
        let page = extract_page(&page());

        assert_eq!(page.content, "# Guide\n\nUseful content.\n\n- First item");
        assert_eq!(page.links[0].url, "https://example.com/docs");
    }

    #[test]
    fn falls_back_to_browser_visible_text() {
        let mut page = page();
        page.blocks.clear();
        assert_eq!(extract_page(&page).content, "Fallback visible text");
    }

    #[tokio::test]
    #[ignore = "requires a Chromium-family browser and network access"]
    async fn live_headless_page_read() {
        let browser = Arc::new(HeadlessBrowser::discover().expect("browser runtime not found"));
        let tool = WebBrowserTool::new(browser);
        let result = tool
            .run(json!({ "url": "https://example.com" }))
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["title"], "Example Domain");
        assert!(
            output["content"]
                .as_str()
                .unwrap()
                .contains("Example Domain")
        );
        assert!(output.get("truncated").is_none());
        tool.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "requires a Chromium-family browser"]
    async fn live_browser_filters_hidden_and_navigation_content() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut visit = 0;
            while visit < 2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let read = socket.read(&mut request).await.unwrap();
                if read == 0 {
                    continue;
                }
                let body = if visit == 0 {
                    r#"<html><head><title>Visibility</title></head><body><main>
                        <h1>Visible heading</h1>
                        <p style="display:none">Hidden text</p>
                        <nav><p>Navigation text</p></nav>
                        <p id="rendered">Before script</p>
                        <script>
                            localStorage.setItem('rope-test', 'Persistent session');
                            document.getElementById('rendered').textContent = 'Rendered text';
                        </script>
                    </main></body></html>"#
                } else {
                    r#"<html><body><main><p id="stored"></p><script>
                        document.getElementById('stored').textContent = localStorage.getItem('rope-test');
                    </script></main></body></html>"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                visit += 1;
            }
        });

        let browser = Arc::new(HeadlessBrowser::discover().expect("browser runtime not found"));
        let tool = WebBrowserTool::new(browser);
        let result = tool
            .run(json!({ "url": format!("http://{address}") }))
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        let content = output["content"].as_str().unwrap();
        assert!(content.contains("Visible heading"));
        assert!(content.contains("Rendered text"));
        assert!(!content.contains("Hidden text"));
        assert!(!content.contains("Navigation text"));
        let result = tool
            .run(json!({ "url": format!("http://{address}/again") }))
            .await
            .unwrap();
        server.await.unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            output["content"]
                .as_str()
                .unwrap()
                .contains("Persistent session")
        );
        tool.shutdown().await;
    }
}
