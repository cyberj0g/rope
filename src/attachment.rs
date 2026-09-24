use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::runtime::{FileContent, MAX_FILE_BYTES, guess_mime_type};

#[derive(serde::Deserialize, serde::Serialize)]
pub struct PreparedFile {
    pub file: FileContent,
    pub prompt: String,
}

pub async fn prepare_file(name: String, bytes: Vec<u8>) -> Result<PreparedFile> {
    let file = tokio::task::spawn_blocking(move || store_file(&name, &bytes)).await??;
    let mut prompt = file_prompt(&file);
    if let Some(preview) = crate::file_processing::preview(
        Path::new(&file.path),
        &[
            &crate::file_processing::ArchiveListing,
            &crate::file_processing::PdfText,
        ],
    )
    .await
    {
        prompt.push_str("\n");
        prompt.push_str(&preview);
    }
    Ok(PreparedFile { file, prompt })
}

pub fn store_file(name: &str, bytes: &[u8]) -> Result<FileContent> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        bail!("file name must be a single filename");
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("file exceeds the 100 MiB limit");
    }
    #[cfg(unix)]
    let root = Path::new("/tmp").to_path_buf();
    #[cfg(not(unix))]
    let root = std::env::temp_dir();
    let directory = tempfile::Builder::new()
        .prefix("rope-upload-")
        .tempdir_in(root)?;
    let path = directory.path().join(name);
    std::fs::write(&path, bytes).context("store uploaded file")?;
    let file = FileContent {
        path: path.to_string_lossy().into_owned(),
        name: name.into(),
        size: bytes.len() as u64,
        mime_type: guess_mime_type(Path::new(name)),
    };
    // keep files available to tools and resumed sessions until the OS cleans /tmp
    let _ = directory.keep();
    Ok(file)
}

pub fn file_prompt(file: &FileContent) -> String {
    format!("Attached file: {}", serde_json::to_string(file).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_preserve_names_without_collisions() {
        let a = store_file("résumé notes.pdf", b"first").unwrap();
        let b = store_file("résumé notes.pdf", b"second").unwrap();
        assert_ne!(a.path, b.path);
        assert_eq!(Path::new(&a.path).file_name().unwrap(), "résumé notes.pdf");
        assert_eq!(std::fs::read(&a.path).unwrap(), b"first");
        assert_eq!(std::fs::read(&b.path).unwrap(), b"second");
        for file in [a, b] {
            std::fs::remove_dir_all(Path::new(&file.path).parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn uploads_reject_paths() {
        for name in [
            "",
            ".",
            "..",
            "../file",
            "/tmp/file",
            "C:\\file",
            "bad\0name",
        ] {
            assert!(store_file(name, b"data").is_err(), "{name:?}");
        }
    }
}
