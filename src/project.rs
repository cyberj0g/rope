use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

#[derive(Clone, Debug, Default)]
pub struct GitFile {
    pub status: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectState {
    pub cwd: PathBuf,
    pub context: Vec<PathBuf>,
    pub git_available: bool,
    pub git_files: Vec<GitFile>,
    pub git_diff: String,
    pub git_diff_path: Option<PathBuf>,
}

impl ProjectState {
    pub async fn new() -> Result<Self> {
        let mut state = Self {
            cwd: std::env::current_dir()?,
            ..Self::default()
        };
        state.refresh().await;
        Ok(state)
    }

    pub async fn add(&mut self, value: &str) -> Result<()> {
        let path = absolute(&self.cwd, value);
        if !path.is_file() {
            bail!("context file not found: {}", path.display());
        }
        if !self.context.contains(&path) {
            self.context.push(path);
            self.context.sort();
        }
        Ok(())
    }

    pub fn drop(&mut self, value: &str) -> Result<()> {
        let path = absolute(&self.cwd, value);
        let before = self.context.len();
        self.context.retain(|entry| entry != &path);
        if self.context.len() == before {
            bail!("file is not in context: {value}");
        }
        Ok(())
    }

    pub async fn prompt(&self) -> Result<Option<String>> {
        let mut sections = Vec::new();
        if let Some(base) = directories::BaseDirs::new() {
            let path = base.config_dir().join("rope/AGENTS.md");
            if path.is_file() {
                sections.push(format!(
                    "Global instructions (~/.config/rope/AGENTS.md):\n{}",
                    tokio::fs::read_to_string(&path).await?
                ));
            }
        }
        let local_agents = self.cwd.join("AGENTS.md");
        if local_agents.is_file() {
            sections.push(format!(
                "Project instructions (AGENTS.md):\n{}",
                tokio::fs::read_to_string(&local_agents).await?
            ));
        }
        if !self.context.is_empty() {
            sections.push(format!(
                "Current working directory: {}\nExplicit context files:",
                self.cwd.display()
            ));
        }
        for path in &self.context {
            let relative = path.strip_prefix(&self.cwd).unwrap_or(path);
            let content = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("read context {}", path.display()))?;
            sections.push(format!("--- {} ---\n{}", relative.display(), content));
        }
        Ok((!sections.is_empty()).then(|| sections.join("\n\n")))
    }

    pub async fn refresh(&mut self) {
        self.git_available = git(&self.cwd, &["rev-parse", "--is-inside-work-tree"])
            .await
            .is_ok();
        if !self.git_available {
            self.git_files.clear();
            self.git_diff.clear();
            self.git_diff_path = None;
            return;
        }
        self.git_files = git(&self.cwd, &["status", "--short"])
            .await
            .unwrap_or_default()
            .lines()
            .filter_map(parse_status_line)
            .collect();
        if let Ok(diff) = diff(&self.cwd, self.git_diff_path.as_deref()).await {
            self.git_diff = diff;
        }
    }

    pub async fn load_diff(&mut self, path: Option<PathBuf>) {
        self.git_diff_path = path;
        self.refresh().await;
    }
}

fn parse_status_line(line: &str) -> Option<GitFile> {
    if line.len() < 4 {
        return None;
    }
    let status = line[..2].to_owned();
    let raw_path = line[3..]
        .rsplit_once(" -> ")
        .map_or(&line[3..], |(_, path)| path);
    Some(GitFile {
        status,
        path: PathBuf::from(raw_path.trim_matches('"')),
    })
}

async fn diff(cwd: &Path, path: Option<&Path>) -> Result<String> {
    let mut args = vec!["diff", "--"];
    if let Some(path) = path {
        args.push(path.to_str().context("git path is not UTF-8")?);
    }
    let mut output = git(cwd, &args).await?;

    let mut cached_args = vec!["diff", "--cached", "--"];
    if let Some(path) = path {
        cached_args.push(path.to_str().context("git path is not UTF-8")?);
    }
    let cached = git(cwd, &cached_args).await?;
    if !cached.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&cached);
    }

    if let Some(path) = path
        && !cwd.join(path).exists()
        && output.is_empty()
    {
        return Ok("File deleted or unavailable; Git has no textual diff for this entry.".into());
    }
    if let Some(path) = path
        && output.is_empty()
        && git(
            cwd,
            &["ls-files", "--error-unmatch", path.to_str().unwrap()],
        )
        .await
        .is_err()
    {
        output = git_allow_failure(
            cwd,
            &[
                "diff",
                "--no-index",
                "--",
                "/dev/null",
                path.to_str().unwrap(),
            ],
        )
        .await?;
    }
    Ok(output)
}

fn absolute(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn git_allow_failure(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_git_status() {
        let file = parse_status_line(" M src/main.rs").unwrap();
        assert_eq!(file.status, " M");
        assert_eq!(file.path, PathBuf::from("src/main.rs"));

        let renamed = parse_status_line("R  old.rs -> new.rs").unwrap();
        assert_eq!(renamed.path, PathBuf::from("new.rs"));
    }
}
