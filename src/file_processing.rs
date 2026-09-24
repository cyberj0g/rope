use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const MAX_PREVIEW_BYTES: usize = 16 * 1024;

#[async_trait]
pub trait FileProcessor: Send + Sync {
    fn name(&self) -> &str;
    fn accepts(&self, path: &Path) -> bool;
    async fn process(&self, path: &Path) -> Result<String>;
}

pub async fn preview(path: &Path, processors: &[&dyn FileProcessor]) -> Option<String> {
    let processor = processors
        .iter()
        .find(|processor| processor.accepts(path))?;
    let result = tokio::time::timeout(Duration::from_secs(10), processor.process(path)).await;
    let text = match result {
        Ok(Ok(mut text)) => {
            if text.len() > MAX_PREVIEW_BYTES {
                let mut end = MAX_PREVIEW_BYTES;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("\n[preview truncated; use the file path for the full content]");
            }
            text
        }
        Ok(Err(error)) => format!("preview unavailable: {error:#}"),
        Err(_) => "preview unavailable: processing exceeded 10 seconds".into(),
    };
    Some(format!(
        "Automatic file preview ({}):\n{text}",
        processor.name()
    ))
}

pub struct ArchiveListing;
pub struct PdfText;

#[async_trait]
impl FileProcessor for ArchiveListing {
    fn name(&self) -> &str {
        "archive listing"
    }

    fn accepts(&self, path: &Path) -> bool {
        let name = path.to_string_lossy().to_ascii_lowercase();
        [
            ".zip", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    }

    async fn process(&self, path: &Path) -> Result<String> {
        let mut command = if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
        {
            let mut command = Command::new("unzip");
            command.args(["-Z", "-1"]);
            command
        } else {
            let mut command = Command::new("tar");
            command.args(["-t", "-f"]);
            command
        };
        command.arg(path);
        command_text(command).await
    }
}

#[async_trait]
impl FileProcessor for PdfText {
    fn name(&self) -> &str {
        "PDF text"
    }

    fn accepts(&self, path: &Path) -> bool {
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
    }

    async fn process(&self, path: &Path) -> Result<String> {
        let mut command = Command::new("pdftotext");
        command
            .args(["-layout", "-enc", "UTF-8"])
            .arg(path)
            .arg("-");
        let text = command_text(command).await?;
        if text.trim().is_empty() {
            return Ok("No embedded text found; this PDF may need OCR.".into());
        }
        Ok(text)
    }
}

async fn read_limited(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64).read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn command_text(mut command: Command) -> Result<String> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {program}; install it to enable this preview"))?;
    let (stdout, stderr) = tokio::try_join!(
        read_limited(child.stdout.take().unwrap(), MAX_PREVIEW_BYTES + 1),
        read_limited(child.stderr.take().unwrap(), 4096),
    )?;
    if stdout.len() > MAX_PREVIEW_BYTES {
        child.kill().await.ok();
    }
    let status = child.wait().await?;
    if !status.success() && stdout.len() <= MAX_PREVIEW_BYTES {
        bail!("{program}: {}", String::from_utf8_lossy(&stderr).trim());
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LongPreview;

    #[async_trait]
    impl FileProcessor for LongPreview {
        fn name(&self) -> &str {
            "test"
        }
        fn accepts(&self, _: &Path) -> bool {
            true
        }
        async fn process(&self, _: &Path) -> Result<String> {
            Ok("é".repeat(MAX_PREVIEW_BYTES))
        }
    }

    #[tokio::test]
    async fn previews_are_bounded_and_processors_are_optional() {
        let path = Path::new("notes.txt");
        assert!(preview(path, &[&ArchiveListing, &PdfText]).await.is_none());
        let text = preview(path, &[&LongPreview]).await.unwrap();
        assert!(text.contains("preview truncated"));
        assert!(text.len() < MAX_PREVIEW_BYTES + 200);
    }

    #[tokio::test]
    async fn lists_archive_without_extracting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.tar");
        let mut archive = tar::Builder::new(std::fs::File::create(&path).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "nested/notes.txt", &b"hello"[..])
            .unwrap();
        archive.finish().unwrap();
        let text = preview(&path, &[&ArchiveListing]).await.unwrap();
        assert!(text.contains("nested/notes.txt"), "{text}");
        assert!(!directory.path().join("nested").exists());
    }
}
