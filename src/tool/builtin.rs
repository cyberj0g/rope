use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use similar::TextDiff;
use tokio::process::Command;

use super::{ExecutionPlan, PlanStatus, Tool, ToolResult};

pub struct ReadTool(pub PathBuf);
pub struct WriteTool(pub PathBuf);
pub struct EditTool(pub PathBuf);
pub struct ShellTool(pub PathBuf);
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

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run a shell command in the current working directory"
    }
    fn schema(&self) -> Value {
        object(json!({ "command": { "type": "string" } }), &["command"])
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let mut command = Command::new("sh");
        command
            .kill_on_drop(true)
            .arg("-c")
            .arg(args.command)
            .current_dir(&self.0);
        let output = command.output().await?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            bail!("command exited with {}\n{text}", output.status);
        }
        Ok(ToolResult {
            output: text,
            image: None,
            diff: None,
        })
    }
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
