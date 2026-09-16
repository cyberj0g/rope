//! Clickable links: URL detection in rendered text and opening links in the
//! default browser.

use std::{io, process::Command, sync::LazyLock};

use regex::Regex;

/// A clickable link in a rendered line. `start` and `end` are character
/// columns — the same basis chat lines are wrapped and selected on — with
/// `end` exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRange {
    pub start: u16,
    pub end: u16,
    pub url: String,
}

/// `http(s)://` URLs and bare `www.` domains. The character class is ASCII
/// on purpose so a match is always one column per character.
static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:https?://|www\.)[A-Za-z0-9\-._~:/?#@!$&*+;=%\[\]()]+").unwrap()
});

/// Normalize a raw link target to a form a browser can open, or `None` when
/// it is not a usable absolute URL. Bare `www.` domains get an `http://`
/// prefix.
pub fn openable_url(raw: &str) -> Option<String> {
    let candidate = if let Some(rest) = raw.strip_prefix("www.") {
        format!("http://www.{rest}")
    } else {
        raw.to_owned()
    };
    url::Url::parse(&candidate).ok().map(|url| url.to_string())
}

/// All URLs in `text` as `(start, end, url)` character ranges with `end`
/// exclusive.
pub fn find_urls(text: &str) -> Vec<(usize, usize, String)> {
    let mut urls = Vec::new();
    for match_ in URL_RE.find_iter(text) {
        let start = match_.start();
        let mut end = match_.end();
        // Trim trailing sentence punctuation and closing brackets with no
        // matching opener inside the match, so prose like `see
        // https://x.com/a(b).` links only the URL.
        while end > start {
            let ch = text.as_bytes()[end - 1] as char;
            let unmatched_closer = match ch {
                ')' => closers_openers(&text[start..end], b')', b'('),
                ']' => closers_openers(&text[start..end], b']', b'['),
                '}' => closers_openers(&text[start..end], b'}', b'{'),
                _ => false,
            };
            let punctuation = matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"');
            if !unmatched_closer && !punctuation {
                break;
            }
            end -= 1;
        }
        let Some(url) = openable_url(&text[start..end]) else {
            continue;
        };
        let start = text[..start].chars().count();
        let end = text[..end].chars().count();
        if end > start {
            urls.push((start, end, url));
        }
    }
    urls
}

fn closers_openers(text: &str, closer: u8, opener: u8) -> bool {
    let (mut closers, mut openers) = (0u32, 0u32);
    for byte in text.bytes() {
        if byte == closer {
            closers += 1;
        } else if byte == opener {
            openers += 1;
        }
    }
    closers > openers
}

/// The URL at character column `column` in `text`, if any.
pub fn url_at(text: &str, column: usize) -> Option<String> {
    find_urls(text)
        .into_iter()
        .find(|(start, end, _)| *start <= column && column < *end)
        .map(|(_, _, url)| url)
}

/// Explicit links (from the markdown renderer) plus every raw URL in the
/// line's text that they do not already cover.
pub fn merge_line_links(text: &str, mut explicit: Vec<LinkRange>) -> Vec<LinkRange> {
    if !text.contains("http") && !text.contains("www.") {
        return explicit;
    }
    for (start, end, url) in find_urls(text) {
        let covered = explicit
            .iter()
            .any(|link| (link.start as usize) < end && start < (link.end as usize));
        if !covered {
            explicit.push(LinkRange {
                start: start as u16,
                end: end as u16,
                url,
            });
        }
    }
    explicit
}

/// Remap per-line links through chat wrapping: `starts` gives the first
/// output line of each input line and every output line holds `width`
/// characters.
pub fn wrap_links(
    links: &[Vec<LinkRange>],
    starts: &[u16],
    width: usize,
    output_len: usize,
) -> Vec<Vec<LinkRange>> {
    let mut wrapped = vec![Vec::new(); output_len];
    for (input, ranges) in links.iter().enumerate() {
        let base = starts[input] as usize;
        let count = starts
            .get(input + 1)
            .map(|&next| (next - starts[input]) as usize)
            .unwrap_or(output_len.saturating_sub(base));
        for range in ranges {
            for fragment in 0..count {
                let lo = fragment * width;
                let hi = lo + width;
                if range.end as usize <= lo || range.start as usize >= hi {
                    continue;
                }
                wrapped[base + fragment].push(LinkRange {
                    start: (range.start as usize).saturating_sub(lo).min(width) as u16,
                    end: (range.end as usize).saturating_sub(lo).min(width) as u16,
                    url: range.url.clone(),
                });
            }
        }
    }
    wrapped
}

/// Open `url` in the default browser (or mail client for `mailto:` links).
pub fn open_in_browser(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).spawn()?;
    }
    #[cfg(windows)]
    {
        // `rundll32` avoids `cmd /C start`, whose re-parsing breaks URLs
        // containing `&` or spaces.
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_http_and_www_urls() {
        let urls = find_urls("see https://example.com/docs and www.example.org/x.");
        assert_eq!(
            urls,
            vec![
                (4, 28, "https://example.com/docs".into()),
                (33, 50, "http://www.example.org/x".into()),
            ]
        );
    }

    #[test]
    fn trims_trailing_punctuation_and_unmatched_brackets() {
        let urls = find_urls("visit (https://x.com/a(b)) and end at https://x.com/v1.0.");
        assert_eq!(
            urls,
            vec![
                (7, 25, "https://x.com/a(b)".into()),
                (38, 56, "https://x.com/v1.0".into()),
            ]
        );
    }

    #[test]
    fn keeps_balanced_brackets_in_wiki_urls() {
        let urls = find_urls("https://en.wikipedia.org/wiki/Foo_(bar)");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "https://en.wikipedia.org/wiki/Foo_(bar)");
    }

    #[test]
    fn ignores_schemeless_and_broken_urls() {
        assert!(find_urls("just a domain name example.com").is_empty());
        assert!(find_urls("see http:// for docs").is_empty());
        assert!(find_urls("xhttp://example.com").is_empty());
    }

    #[test]
    fn url_at_hits_by_column() {
        let text = "open https://x.com now";
        assert_eq!(url_at(text, 6).as_deref(), Some("https://x.com/"));
        assert_eq!(url_at(text, 14).as_deref(), Some("https://x.com/"));
        assert_eq!(url_at(text, 18), None);
        assert_eq!(url_at(text, 0), None);
    }

    #[test]
    fn openable_url_normalizes_and_validates() {
        assert_eq!(
            openable_url("www.example.org/x").as_deref(),
            Some("http://www.example.org/x")
        );
        assert_eq!(
            openable_url("https://example.com").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(openable_url("relative/path"), None);
        assert_eq!(openable_url("#anchor"), None);
    }

    #[test]
    fn merge_keeps_explicit_and_adds_raw_urls() {
        let text = "docs and https://y.com";
        let explicit = vec![LinkRange {
            start: 0,
            end: 4,
            url: "https://x.com/".into(),
        }];
        let merged = merge_line_links(text, explicit);
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged[1],
            LinkRange {
                start: 9,
                end: 22,
                url: "https://y.com/".into()
            }
        );
    }

    #[test]
    fn merge_skips_raw_url_inside_explicit_link() {
        let text = "see https://x.com ok";
        let explicit = vec![LinkRange {
            start: 4,
            end: 16,
            url: "https://x.com/".into(),
        }];
        let merged = merge_line_links(text, explicit);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn wrap_splits_links_across_lines() {
        let links = vec![vec![LinkRange {
            start: 7,
            end: 13,
            url: "https://x.com/".into(),
        }]];
        let wrapped = wrap_links(&links, &[0, 2], 10, 2);
        assert_eq!(
            wrapped[0],
            vec![LinkRange {
                start: 7,
                end: 10,
                url: "https://x.com/".into()
            }]
        );
        assert_eq!(
            wrapped[1],
            vec![LinkRange {
                start: 0,
                end: 3,
                url: "https://x.com/".into()
            }]
        );
    }
}
