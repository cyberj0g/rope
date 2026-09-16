use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::builtin::{object, path};
use super::{Tool, ToolResult};

/// One heading in an Org outline: its level, full heading text, and the
/// inclusive 1-based line range of its complete subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgNode {
    pub level: usize,
    pub title: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// The heading hierarchy of an Org file, in source order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrgOutline {
    pub line_count: usize,
    pub nodes: Vec<OrgNode>,
}

const BEGIN_PREFIX: &str = "#+begin_";
const END_PREFIX: &str = "#+end_";

/// Scans `content` once, recording headings outside `#+begin_...#+end_`
/// blocks and deriving each subtree's `end_line` from the next heading of
/// the same or higher level. No Org AST is built: memory grows with the
/// number of headings, and the scan is linear in the file.
pub fn extract_outline(content: &str) -> OrgOutline {
    if content.is_empty() {
        return OrgOutline::default();
    }
    // A trailing newline does not open an extra line, but a lone "\n" is
    // still one (empty) line.
    let body = content.strip_suffix('\n').unwrap_or(content);
    let line_count = body.split('\n').count();

    let mut nodes: Vec<OrgNode> = Vec::new();
    // Innermost-open block kind; begin markers push, matching end markers pop.
    let mut blocks: Vec<&str> = Vec::new();

    for (line_no, raw) in body.split('\n').enumerate() {
        let line_no = line_no + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if let Some(keyword) = block_keyword(line, BEGIN_PREFIX) {
            blocks.push(keyword);
            continue;
        }
        if let Some(keyword) = block_keyword(line, END_PREFIX) {
            // Close the innermost block of the same kind; stray end
            // markers outside a matching block are ignored.
            if blocks
                .last()
                .is_some_and(|open| open.eq_ignore_ascii_case(keyword))
            {
                blocks.pop();
            }
            continue;
        }
        if blocks.is_empty()
            && let Some((level, title)) = heading(line)
        {
            nodes.push(OrgNode {
                level,
                title: title.to_owned(),
                start_line: line_no,
                end_line: 0,
            });
        }
    }

    // A subtree ends on the line before the next heading of the same or
    // higher level, or at the last line of the file. The open-stack keeps
    // strictly increasing levels, so both passes stay O(number of headings).
    let mut open: Vec<usize> = Vec::new();
    for index in 0..nodes.len() {
        while let Some(&top) = open.last() {
            if nodes[index].level <= nodes[top].level {
                nodes[top].end_line = nodes[index].start_line - 1;
                open.pop();
            } else {
                break;
            }
        }
        open.push(index);
    }
    for index in open {
        nodes[index].end_line = line_count;
    }

    OrgOutline { line_count, nodes }
}

/// The Org keyword of a `#+begin_...` / `#+end_...` marker, if the line
/// starts with `prefix` (case-insensitively). Only the first word after
/// the underscore counts, so `#+begin_src python` yields `src`.
fn block_keyword<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    // get() also rejects a range that would cut a multi-byte character.
    let head = line.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    line[prefix.len()..].split_whitespace().next()
}

/// A heading at column 0: one or more `*`, then at least one whitespace
/// character. Returns its level and the complete heading text after the
/// stars; Org semantics (TODO, priorities, tags) are left untouched.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|byte| *byte == b'*').count();
    if level == 0 {
        return None;
    }
    let rest = &line[level..];
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    Some((
        level,
        rest.trim_start_matches(char::is_whitespace)
            .trim_end_matches(char::is_whitespace),
    ))
}

pub struct OrgOutlineTool(pub PathBuf);

#[async_trait]
impl Tool for OrgOutlineTool {
    fn name(&self) -> &str {
        "org_outline"
    }
    fn description(&self) -> &str {
        "Return the Org-mode heading hierarchy for a file, including line ranges for each subtree. Use this to navigate large Org files before reading or editing specific sections. Apply only to .org files."
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "path": {
                    "type": "string",
                    "description": "Path to the Org-mode file."
                }
            }),
            &["path"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let target = path(&self.0, &args.path);
        let bytes = match tokio::fs::read(&target).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(tool_error(
                    "file_not_found",
                    &format!("File does not exist: {}", target.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Ok(tool_error(
                    "permission_denied",
                    &format!("Cannot read file: {}", target.display()),
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                return Ok(tool_error("invalid_encoding", "File is not valid UTF-8."));
            }
        };
        let outline = extract_outline(&content);
        Ok(ToolResult {
            output: render(&target.to_string_lossy(), &outline),
            image: None,
            diff: None,
        })
    }
}

/// One compact node object per line, so the context cost scales with the
/// number of headings rather than with fixed pretty-printing overhead.
fn render(path: &str, outline: &OrgOutline) -> String {
    let mut out = String::from("{\n");
    out.push_str(&format!("  \"path\": {},\n", json!(path)));
    out.push_str(&format!("  \"line_count\": {},\n", outline.line_count));
    out.push_str("  \"nodes\": [");
    for (index, node) in outline.nodes.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str("    ");
        out.push_str(&node_json(node));
    }
    if !outline.nodes.is_empty() {
        out.push('\n');
    }
    out.push_str("  ]\n}");
    out
}

/// One node object in the spec's key order; only the title needs JSON
/// escaping, so it is the only part delegated to serde_json.
fn node_json(node: &OrgNode) -> String {
    format!(
        "{{\"level\":{},\"title\":{},\"start_line\":{},\"end_line\":{}}}",
        node.level,
        json!(node.title),
        node.start_line,
        node.end_line,
    )
}

fn tool_error(code: &str, message: &str) -> ToolResult {
    ToolResult {
        output: json!({ "error": code, "message": message }).to_string(),
        image: None,
        diff: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_subtree_ranges_in_source_order() {
        let content = "* A\ntext\n** B\ntext\n** C\ntext\n* D\n";
        let outline = extract_outline(content);

        assert_eq!(outline.line_count, 7);
        assert_eq!(
            outline.nodes,
            vec![
                OrgNode {
                    level: 1,
                    title: "A".into(),
                    start_line: 1,
                    end_line: 6
                },
                OrgNode {
                    level: 2,
                    title: "B".into(),
                    start_line: 3,
                    end_line: 4
                },
                OrgNode {
                    level: 2,
                    title: "C".into(),
                    start_line: 5,
                    end_line: 6
                },
                OrgNode {
                    level: 1,
                    title: "D".into(),
                    start_line: 7,
                    end_line: 7
                },
            ]
        );
    }

    #[test]
    fn preamble_and_trailing_text_are_not_nodes() {
        let content = "#+title: Notes\nSome introduction.\n\n* Work\n** Rope\ndetails\n";
        let outline = extract_outline(content);

        assert_eq!(outline.line_count, 6);
        assert_eq!(
            outline.nodes,
            vec![
                OrgNode {
                    level: 1,
                    title: "Work".into(),
                    start_line: 4,
                    end_line: 6
                },
                OrgNode {
                    level: 2,
                    title: "Rope".into(),
                    start_line: 5,
                    end_line: 6
                },
            ]
        );
    }

    #[test]
    fn keeps_full_heading_text_without_parsing_org_semantics() {
        let content = "*** TODO [#A] Implement parser :rope:rust:  \n";
        let outline = extract_outline(content);

        assert_eq!(outline.nodes.len(), 1);
        assert_eq!(outline.nodes[0].level, 3);
        assert_eq!(
            outline.nodes[0].title,
            "TODO [#A] Implement parser :rope:rust:"
        );
    }

    #[test]
    fn rejects_non_headings() {
        let outline = extract_outline("some * text\n*invalid\n  * indented\n* \n");
        // Only the empty-titled `* ` is a real heading.
        assert_eq!(
            outline.nodes,
            vec![OrgNode {
                level: 1,
                title: String::new(),
                start_line: 4,
                end_line: 4
            }]
        );
    }

    #[test]
    fn ignores_heading_like_text_inside_blocks() {
        let content = concat!(
            "* Real\n",
            "#+begin_src text\n",
            "* not a heading\n",
            "** still not\n",
            "#+end_src\n",
            "#+BEGIN_QUOTE\n",
            "* also not\n",
            "#+end_quote\n",
            "#+begin_example\n",
            "* unterminated swallows the rest\n"
        );
        let outline = extract_outline(content);

        assert_eq!(outline.line_count, 10);
        assert_eq!(
            outline.nodes,
            vec![OrgNode {
                level: 1,
                title: "Real".into(),
                start_line: 1,
                end_line: 10
            }]
        );
    }

    #[test]
    fn nested_blocks_close_innermost_first() {
        let content = concat!(
            "#+begin_quote\n",
            "#+begin_example\n",
            "* hidden\n",
            "#+end_example\n",
            "* still hidden\n",
            "#+end_quote\n",
            "* visible\n"
        );
        let outline = extract_outline(content);

        assert_eq!(
            outline.nodes,
            vec![OrgNode {
                level: 1,
                title: "visible".into(),
                start_line: 7,
                end_line: 7
            }]
        );
    }

    #[test]
    fn level_jumps_end_every_open_subtree() {
        let content = "* A\n*** C\ntext\n* D\n";
        let outline = extract_outline(content);

        assert_eq!(
            outline.nodes,
            vec![
                OrgNode {
                    level: 1,
                    title: "A".into(),
                    start_line: 1,
                    end_line: 3
                },
                OrgNode {
                    level: 3,
                    title: "C".into(),
                    start_line: 2,
                    end_line: 3
                },
                OrgNode {
                    level: 1,
                    title: "D".into(),
                    start_line: 4,
                    end_line: 4
                },
            ]
        );
    }

    #[test]
    fn handles_empty_files_and_crlf_line_endings() {
        assert_eq!(extract_outline(""), OrgOutline::default());
        assert_eq!(extract_outline("\n").line_count, 1);

        let outline = extract_outline("* A\r\ntext\r\n** B\r\n");
        assert_eq!(outline.line_count, 3);
        assert_eq!(
            outline.nodes,
            vec![
                OrgNode {
                    level: 1,
                    title: "A".into(),
                    start_line: 1,
                    end_line: 3
                },
                OrgNode {
                    level: 2,
                    title: "B".into(),
                    start_line: 3,
                    end_line: 3
                },
            ]
        );
    }

    #[test]
    fn lines_cutting_non_ascii_characters_near_a_block_prefix_do_not_panic() {
        // The multi-byte character starts at byte 7, so slicing the first
        // eight bytes would cut it.
        let outline = extract_outline("#+begin\u{435}x\n* A\n");

        assert_eq!(outline.line_count, 2);
        assert_eq!(outline.nodes.len(), 1);
        assert_eq!(outline.nodes[0].title, "A");
    }

    #[test]
    fn render_is_valid_json_with_compact_nodes() {
        let outline = OrgOutline {
            line_count: 4821,
            nodes: vec![
                OrgNode {
                    level: 1,
                    title: "Work".into(),
                    start_line: 10,
                    end_line: 950,
                },
                OrgNode {
                    level: 2,
                    title: "Rope \"quoted\"".into(),
                    start_line: 40,
                    end_line: 310,
                },
            ],
        };
        let rendered = render("/home/user/notes.org", &outline);

        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["path"], "/home/user/notes.org");
        assert_eq!(value["line_count"], 4821);
        assert_eq!(value["nodes"][0]["title"], "Work");
        assert_eq!(value["nodes"][1]["title"], "Rope \"quoted\"");
        assert_eq!(value["nodes"][1]["end_line"], 310);
        // One node object per line.
        assert!(rendered.contains("    {\"level\":1"));
    }

    #[tokio::test]
    async fn missing_file_returns_a_structured_error() {
        let root = std::env::temp_dir().join(format!(
            "rope-org-outline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();

        let tool = OrgOutlineTool(root.clone());
        let result = tool.run(json!({ "path": "missing.org" })).await.unwrap();
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(value["error"], "file_not_found");
        assert!(value["message"].as_str().unwrap().contains("missing.org"));

        tokio::fs::write(root.join("bad.org"), [0xFF, 0xFE, b'*'])
            .await
            .unwrap();
        let result = tool.run(json!({ "path": "bad.org" })).await.unwrap();
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(value["error"], "invalid_encoding");
        assert_eq!(value["message"], "File is not valid UTF-8.");

        tokio::fs::write(root.join("notes.org"), "* Top\n** Sub\ncontent\n")
            .await
            .unwrap();
        let result = tool.run(json!({ "path": "notes.org" })).await.unwrap();
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(value["line_count"], 3);
        assert_eq!(value["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["nodes"][0],
            json!({ "level": 1, "title": "Top", "start_line": 1, "end_line": 3 })
        );
        assert_eq!(
            value["nodes"][1],
            json!({ "level": 2, "title": "Sub", "start_line": 2, "end_line": 3 })
        );

        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
