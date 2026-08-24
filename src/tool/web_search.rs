use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use super::{
    Tool, ToolResult,
    headless::{BrowserPage, HeadlessBrowser},
};

pub struct WebSearchTool {
    browser: HeadlessBrowser,
}

#[derive(Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[derive(Clone, Copy)]
enum Provider {
    DuckDuckGo,
    Bing,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Bing => "bing",
        }
    }

    fn url(self, query: &str) -> Url {
        let base = match self {
            Self::DuckDuckGo => "https://duckduckgo.com/",
            Self::Bing => "https://www.bing.com/search",
        };
        let mut url = Url::parse(base).unwrap();
        url.query_pairs_mut().append_pair("q", query);
        if matches!(self, Self::DuckDuckGo) {
            url.query_pairs_mut().append_pair("ia", "web");
        }
        url
    }

    fn parse(self, html: &str, limit: usize) -> Vec<SearchResult> {
        match self {
            Self::DuckDuckGo => parse_duckduckgo(html, limit),
            Self::Bing => parse_bing(html, limit),
        }
    }
}

impl WebSearchTool {
    pub fn discover() -> Option<Self> {
        HeadlessBrowser::discover().map(|browser| Self { browser })
    }

    async fn search(&self, provider: Provider, query: &str) -> Result<BrowserPage> {
        self.browser
            .load(&provider.url(query))
            .await
            .with_context(|| format!("{} search", provider.name()))
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web in a headless browser and return organic result titles, URLs, and snippets"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 5
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default = "default_max_results")]
            max_results: usize,
        }

        let args: Args = serde_json::from_value(args)?;
        let query = args.query.trim();
        if query.is_empty() {
            bail!("search query is empty");
        }
        if !(1..=10).contains(&args.max_results) {
            bail!("max_results must be between 1 and 10");
        }

        let mut errors = Vec::new();
        for provider in [Provider::DuckDuckGo, Provider::Bing] {
            match self.search(provider, query).await {
                Ok(page) => {
                    let results = provider.parse(&page.html, args.max_results);
                    if !results.is_empty() {
                        return Ok(ToolResult {
                            output: serde_json::to_string_pretty(&json!({
                                "query": query,
                                "provider": provider.name(),
                                "results": results,
                            }))?,
                            image: None,
                        });
                    }
                    errors.push(format!("{} returned no organic results", provider.name()));
                }
                Err(error) => errors.push(format!("{}: {error:#}", provider.name())),
            }
        }
        bail!("web search failed: {}", errors.join("; "))
    }
}

fn default_max_results() -> usize {
    5
}

fn parse_duckduckgo(html: &str, limit: usize) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let items = Selector::parse("article[data-testid='result'], .result").unwrap();
    let links = Selector::parse("a[data-testid='result-title-a'], a.result__a").unwrap();
    let modern_snippet =
        Selector::parse("article[data-testid='result'] > div:nth-child(4)").unwrap();
    let lite_snippet = Selector::parse(".result__snippet").unwrap();
    collect_results(document.select(&items), &links, limit, |item| {
        item.select(&lite_snippet)
            .next()
            .or_else(|| item.select(&modern_snippet).next())
            .map(text)
            .unwrap_or_default()
    })
}

fn parse_bing(html: &str, limit: usize) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let items = Selector::parse("li.b_algo").unwrap();
    let links = Selector::parse("h2 a").unwrap();
    let snippets = Selector::parse(".b_caption p").unwrap();
    collect_results(document.select(&items), &links, limit, |item| {
        item.select(&snippets).next().map(text).unwrap_or_default()
    })
}

fn collect_results<'a>(
    items: impl Iterator<Item = ElementRef<'a>>,
    links: &Selector,
    limit: usize,
    snippet: impl Fn(ElementRef<'a>) -> String,
) -> Vec<SearchResult> {
    let mut seen = HashSet::new();
    items
        .filter_map(|item| {
            let link = item.select(links).next()?;
            let title = text(link);
            let url = result_url(link.value().attr("href")?)?;
            if title.is_empty() || !seen.insert(url.clone()) {
                return None;
            }
            Some(SearchResult {
                title,
                url,
                snippet: snippet(item),
            })
        })
        .take(limit)
        .collect()
}

fn result_url(href: &str) -> Option<String> {
    let url = Url::parse(href).ok()?;
    if url
        .host_str()
        .is_some_and(|host| host.ends_with("duckduckgo.com"))
        && let Some(destination) = url.query_pairs().find(|(key, _)| key == "uddg")
    {
        let destination = destination.1.into_owned();
        return is_web_url(&destination).then_some(destination);
    }
    if url.host_str() == Some("www.bing.com") {
        let encoded = url.query_pairs().find(|(key, _)| key == "u")?.1;
        let encoded = encoded.strip_prefix("a1").unwrap_or(&encoded);
        let decoded = URL_SAFE_NO_PAD.decode(encoded.as_bytes()).ok()?;
        return String::from_utf8(decoded)
            .ok()
            .filter(|url| is_web_url(url));
    }
    is_web_url(href).then(|| href.to_owned())
}

fn is_web_url(value: &str) -> bool {
    Url::parse(value)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn text(element: ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_results() {
        let html = r#"
            <article data-testid="result">
              <a data-testid="result-title-a" href="https://www.rust-lang.org/">Rust Language</a>
              <div></div><div></div><div>Fast, safe, and productive.</div>
            </article>
        "#;
        let results = parse_duckduckgo(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].snippet, "Fast, safe, and productive.");
    }

    #[test]
    fn parses_bing_redirects_and_limits_results() {
        let html = r#"
            <li class="b_algo"><h2><a href="https://www.bing.com/ck/a?u=a1aHR0cHM6Ly9ydXN0LWxhbmcub3JnLw">Rust</a></h2><div class="b_caption"><p>Rust language</p></div></li>
            <li class="b_algo"><h2><a href="https://example.com/other">Other</a></h2></li>
        "#;
        let results = parse_bing(html, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(results[0].snippet, "Rust language");
    }

    #[test]
    fn rejects_non_web_result_urls() {
        assert_eq!(result_url("javascript:alert(1)"), None);
        assert_eq!(
            result_url("https://example.com"),
            Some("https://example.com".into())
        );
    }

    #[test]
    fn unwraps_duckduckgo_redirects() {
        assert_eq!(
            result_url("https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs"),
            Some("https://example.com/docs".into())
        );
    }

    #[tokio::test]
    #[ignore = "requires a Chromium-family browser and network access"]
    async fn live_headless_search() {
        let tool = WebSearchTool::discover().expect("Chromium-family browser not found");
        let result = tool
            .run(json!({ "query": "Rust programming language", "max_results": 3 }))
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["results"].as_array().unwrap().len(), 3);
        assert!(
            output["results"][0]["url"]
                .as_str()
                .unwrap()
                .starts_with("http")
        );
        assert!(output["results"].as_array().unwrap().iter().any(|result| {
            result["snippet"]
                .as_str()
                .is_some_and(|snippet| !snippet.is_empty())
        }));
    }
}
