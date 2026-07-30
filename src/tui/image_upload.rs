//! Validation and durable storage for images attached to TUI text inputs.
//!
//! Screens retain [`ImageAttachment`] values with their draft state. The
//! bytes themselves are content-addressed and stored outside a worktree, so a
//! resumed flow can continue to pass the path to an AI harness without placing
//! binary data in a prompt or leaving untracked files behind.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::constants::{
    image_uploads_dir, IMAGE_UPLOAD_CLEANUP_LIMIT, IMAGE_UPLOAD_MAX_BYTES,
    IMAGE_UPLOAD_RETENTION_DAYS,
};

/// A durable reference to an image uploaded from a clipboard or dropped file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    /// BLAKE3 digest of the original image bytes.
    pub id: String,
    /// Content-addressed filename, suitable for displaying or passing to a CLI.
    pub filename: String,
    pub mime_type: String,
    pub path: PathBuf,
}

/// Image data returned by a platform clipboard reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
}

/// The result relevant to prompt attachment handling from a clipboard reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardRead {
    Empty,
    Image(ClipboardImage),
}

#[derive(Debug, Error)]
pub enum ImageUploadError {
    #[error("No image was found in the clipboard.")]
    EmptyClipboard,
    #[error("No image path was provided.")]
    EmptyPath,
    #[error("The dropped item is not a file: {}", .0.display())]
    NotAFile(PathBuf),
    #[error("Could not read image {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The supported set is the intersection of what OpenCode, Codex, and
    /// Claude Code all accept. Codex rejects BMP/TIFF outright and Claude's
    /// vision API accepts only these four media types.
    #[error("This image format is not supported. Use PNG, JPEG, GIF, or WebP.")]
    UnsupportedFormat,
    #[error("The image is corrupt or cannot be decoded.")]
    CorruptImage,
    #[error("The image is {size_mb:.1} MB; the {limit_mb} MB limit keeps it within what the AI harnesses accept.")]
    TooLarge { size_mb: f64, limit_mb: u64 },
    #[error("Could not store the image: {0}")]
    Storage(#[source] std::io::Error),
    #[error("Invalid file URI: {0}")]
    InvalidFileUri(String),
}

/// Content-addressed image storage. A custom root is useful for tests and
/// keeps this component independent from any one TUI screen.
#[derive(Debug, Clone)]
pub struct ImageStorage {
    root: PathBuf,
}

impl Default for ImageStorage {
    fn default() -> Self {
        Self::new(image_uploads_dir())
    }
}

impl ImageStorage {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store clipboard bytes after confirming they are a supported, decodable image.
    pub fn ingest_clipboard(
        &self,
        clipboard: ClipboardRead,
    ) -> Result<ImageAttachment, ImageUploadError> {
        match clipboard {
            ClipboardRead::Empty => Err(ImageUploadError::EmptyClipboard),
            ClipboardRead::Image(image) => self.ingest_bytes(&image.bytes),
        }
    }

    /// Copy a dropped local image into durable Wisetree-owned storage.
    pub fn ingest_file(&self, path: impl AsRef<Path>) -> Result<ImageAttachment, ImageUploadError> {
        let path = path.as_ref();
        let metadata = fs::metadata(path).map_err(|source| ImageUploadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(ImageUploadError::NotAFile(path.to_path_buf()));
        }
        let bytes = fs::read(path).map_err(|source| ImageUploadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        self.ingest_bytes(&bytes)
    }

    /// Process every dropped path independently; callers can show failures
    /// while retaining valid attachments from the same terminal paste/drop.
    pub fn ingest_dropped_paths(
        &self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Vec<Result<ImageAttachment, ImageUploadError>> {
        paths
            .into_iter()
            .map(|path| self.ingest_file(path))
            .collect()
    }

    pub fn ingest_bytes(&self, bytes: &[u8]) -> Result<ImageAttachment, ImageUploadError> {
        let (extension, mime_type) = identify_image(bytes)?;
        let limit = IMAGE_UPLOAD_MAX_BYTES;
        if bytes.len() as u64 > limit {
            return Err(ImageUploadError::TooLarge {
                size_mb: bytes.len() as f64 / (1024.0 * 1024.0),
                limit_mb: limit / (1024 * 1024),
            });
        }
        let id = blake3::hash(bytes).to_hex().to_string();
        let filename = format!("{id}.{extension}");
        let path = self.root.join(&filename);

        fs::create_dir_all(&self.root).map_err(ImageUploadError::Storage)?;
        if !path.exists() {
            let temporary = self
                .root
                .join(format!(".{filename}.tmp-{}", std::process::id()));
            fs::write(&temporary, bytes).map_err(ImageUploadError::Storage)?;
            if let Err(error) = fs::rename(&temporary, &path) {
                let _ = fs::remove_file(&temporary);
                return Err(ImageUploadError::Storage(error));
            }
        }

        // Ensure an existing content-addressed file still is a usable reference.
        fs::File::open(&path).map_err(ImageUploadError::Storage)?;
        Ok(ImageAttachment {
            id,
            filename,
            mime_type: mime_type.to_string(),
            path,
        })
    }

    /// Remove a bounded number of old upload files that are not referenced by
    /// a live draft or resumable workflow. Missing files and malformed
    /// directory entries are intentionally ignored: cleanup is best-effort
    /// and must never prevent a prompt from being resumed.
    pub fn cleanup_unreferenced(
        &self,
        referenced: impl IntoIterator<Item = PathBuf>,
    ) -> std::io::Result<usize> {
        self.cleanup_unreferenced_at(referenced, SystemTime::now())
    }

    /// Find references written into active `PLAN.md` and
    /// `BUG_INVESTIGATION.md` workflow files before cleanup. Partial or
    /// malformed metadata is ignored conservatively: no valid reference is
    /// needed before an unrelated stale upload can become eligible.
    pub fn cleanup_unreferenced_in_worktrees(
        &self,
        worktrees: impl IntoIterator<Item = PathBuf>,
    ) -> std::io::Result<usize> {
        let mut referenced = Vec::new();
        for worktree in worktrees {
            for name in ["PLAN.md", "BUG_INVESTIGATION.md"] {
                let Ok(content) = fs::read_to_string(worktree.join(name)) else {
                    continue;
                };
                let Some(paths) = referenced_paths(&content) else {
                    // An incomplete resumable file might still refer to an
                    // attachment. Preserve the store until it is repaired or
                    // removed rather than risk losing that context.
                    return Ok(0);
                };
                referenced.extend(paths);
            }
        }
        self.cleanup_unreferenced(referenced)
    }

    fn cleanup_unreferenced_at(
        &self,
        referenced: impl IntoIterator<Item = PathBuf>,
        now: SystemTime,
    ) -> std::io::Result<usize> {
        let referenced: HashSet<_> = referenced.into_iter().collect();
        let retention = Duration::from_secs(IMAGE_UPLOAD_RETENTION_DAYS * 24 * 60 * 60);
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Ok(0);
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            if removed == IMAGE_UPLOAD_CLEANUP_LIMIT {
                break;
            }
            let path = entry.path();
            if referenced.contains(&path) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let old_enough = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= retention);
            if !metadata.is_file() || !old_enough {
                continue;
            }
            match fs::remove_file(path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {}
            }
        }
        Ok(removed)
    }
}

fn referenced_paths(content: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for prefix in [
        "<!-- wisetree:image-attachments:",
        "<!-- wisetree-bug-attachments: ",
    ] {
        let Some(json) = content
            .lines()
            .find_map(|line| line.trim().strip_prefix(prefix))
        else {
            continue;
        };
        let attachments =
            serde_json::from_str::<Vec<ImageAttachment>>(json.trim_end_matches(" -->").trim())
                .ok()?;
        paths.extend(attachments.into_iter().map(|attachment| attachment.path));
    }
    Some(paths)
}

fn identify_image(bytes: &[u8]) -> Result<(&'static str, &'static str), ImageUploadError> {
    // Structural checks reject truncated/corrupt clipboard blobs without a
    // heavyweight decoder dependency in the TUI input path.
    let detected = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        valid_png(bytes).then_some(("png", "image/png"))
    } else if bytes.starts_with(b"\xff\xd8") {
        (bytes.len() >= 4 && bytes.ends_with(b"\xff\xd9")).then_some(("jpg", "image/jpeg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        (bytes.len() >= 14 && bytes.last() == Some(&0x3b)).then_some(("gif", "image/gif"))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        (bytes.len() >= 12
            && u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8 == bytes.len())
        .then_some(("webp", "image/webp"))
    } else {
        // BMP, ICO, and TIFF are deliberately absent: Codex rejects them and
        // Claude's vision API accepts only the four types above, so accepting
        // them here would only defer the failure to the AI run.
        return Err(ImageUploadError::UnsupportedFormat);
    };
    detected.ok_or(ImageUploadError::CorruptImage)
}

fn valid_png(bytes: &[u8]) -> bool {
    let mut offset = 8;
    let mut first = true;
    while offset + 12 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        if first && (kind != b"IHDR" || length != 13) {
            return false;
        }
        let Some(next) = offset.checked_add(12 + length) else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        if kind == b"IEND" {
            return length == 0 && next == bytes.len();
        }
        first = false;
        offset = next;
    }
    false
}

/// Parse a terminal-provided file list. It accepts newline-separated paths,
/// shell quotes/backslash escaping, and percent-encoded `file://` URIs.
pub fn parse_dropped_paths(value: &str) -> Result<Vec<PathBuf>, ImageUploadError> {
    let mut paths = Vec::new();
    for line in value.lines().filter(|line| !line.trim().is_empty()) {
        for token in shell_tokens(line)? {
            paths.push(file_uri_to_path(&token)?);
        }
    }
    if paths.is_empty() {
        Err(ImageUploadError::EmptyPath)
    } else {
        Ok(paths)
    }
}

fn shell_tokens(input: &str) -> Result<Vec<String>, ImageUploadError> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                token.push(ch);
            }
        } else if ch.is_whitespace() && quote.is_none() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if escaped {
        token.push('\\');
    }
    if quote.is_some() {
        return Err(ImageUploadError::InvalidFileUri(
            "unterminated quoted path".to_string(),
        ));
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

fn file_uri_to_path(value: &str) -> Result<PathBuf, ImageUploadError> {
    let Some(rest) = value.strip_prefix("file://") else {
        return Ok(PathBuf::from(value));
    };
    let path = if let Some(path) = rest.strip_prefix('/') {
        format!("/{path}")
    } else if let Some(path) = rest.strip_prefix("localhost/") {
        format!("/{path}")
    } else {
        return Err(ImageUploadError::InvalidFileUri(value.to_string()));
    };
    Ok(PathBuf::from(percent_decode(&path).ok_or_else(|| {
        ImageUploadError::InvalidFileUri(value.to_string())
    })?))
}

fn percent_decode(value: &str) -> Option<String> {
    let mut output = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let low = (*bytes.get(i + 2)? as char).to_digit(16)?;
            output.push((high * 16 + low) as u8);
            i += 3;
        } else {
            output.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(output).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // A valid 1×1 PNG.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\x9cc\xf8\xcf\xc0\xf0\x1f\0\x05\0\x01\xff\x89\x99=\x1d\0\0\0\0IEND\xaeB`\x82";

    #[test]
    fn clipboard_results_are_validated_and_stored_once() {
        let storage = ImageStorage::new(tempdir().unwrap().path().join("uploads"));
        assert!(matches!(
            storage.ingest_clipboard(ClipboardRead::Empty),
            Err(ImageUploadError::EmptyClipboard)
        ));
        let first = storage
            .ingest_clipboard(ClipboardRead::Image(ClipboardImage {
                bytes: PNG.to_vec(),
            }))
            .unwrap();
        let second = storage.ingest_bytes(PNG).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.mime_type, "image/png");
        assert!(first.path.is_file());
        assert_eq!(fs::read_dir(storage.root()).unwrap().count(), 1);
    }

    #[test]
    fn parses_quoted_escaped_and_file_uri_paths() {
        assert_eq!(
            parse_dropped_paths("'/tmp/a b.png' /tmp/c\\ d.png\nfile:///tmp/e%20f.png").unwrap(),
            vec![
                PathBuf::from("/tmp/a b.png"),
                PathBuf::from("/tmp/c d.png"),
                PathBuf::from("/tmp/e f.png")
            ]
        );
        assert!(matches!(
            parse_dropped_paths("file://example.com/a.png"),
            Err(ImageUploadError::InvalidFileUri(_))
        ));
    }

    #[test]
    fn bad_drop_does_not_discard_good_ones() {
        let dir = tempdir().unwrap();
        let good = dir.path().join("good.png");
        fs::write(&good, PNG).unwrap();
        let results = ImageStorage::new(dir.path().join("uploads"))
            .ingest_dropped_paths(vec![good, dir.path().to_path_buf()]);
        assert!(results[0].is_ok());
        assert!(matches!(results[1], Err(ImageUploadError::NotAFile(_))));
    }

    #[test]
    fn corrupt_and_unwritable_storage_fail() {
        let dir = tempdir().unwrap();
        let storage = ImageStorage::new(dir.path().join("uploads"));
        assert!(matches!(
            storage.ingest_bytes(b"\x89PNG\r\n\x1a\nnot an image"),
            Err(ImageUploadError::CorruptImage)
        ));
        let root_file = dir.path().join("not-a-directory");
        fs::write(&root_file, "x").unwrap();
        assert!(matches!(
            ImageStorage::new(root_file).ingest_bytes(PNG),
            Err(ImageUploadError::Storage(_))
        ));
    }

    #[test]
    fn cleanup_keeps_referenced_uploads_and_tolerates_partial_storage() {
        let dir = tempdir().unwrap();
        let storage = ImageStorage::new(dir.path().join("uploads"));
        let kept = storage.ingest_bytes(PNG).unwrap();
        let stale = storage.root().join("stale.png");
        fs::write(&stale, PNG).unwrap();
        let partial = storage.root().join("partial.tmp");
        fs::create_dir(&partial).unwrap();

        let after_retention = SystemTime::now()
            .checked_add(Duration::from_secs(
                (IMAGE_UPLOAD_RETENTION_DAYS + 1) * 24 * 60 * 60,
            ))
            .unwrap();
        assert_eq!(
            storage
                .cleanup_unreferenced_at([kept.path.clone()], after_retention)
                .unwrap(),
            1
        );
        assert!(kept.path.exists());
        assert!(!stale.exists());
        assert!(partial.exists());
    }

    #[test]
    fn cleanup_reads_live_plan_and_investigation_references() {
        let dir = tempdir().unwrap();
        let storage = ImageStorage::new(dir.path().join("uploads"));
        let attachment = storage.ingest_bytes(PNG).unwrap();
        fs::write(
            dir.path().join("PLAN.md"),
            format!(
                "<!-- wisetree:image-attachments:{} -->",
                serde_json::json!([attachment])
            ),
        )
        .unwrap();
        assert_eq!(
            storage
                .cleanup_unreferenced_in_worktrees([dir.path().to_path_buf()])
                .unwrap(),
            0
        );
        assert!(storage.root().read_dir().unwrap().next().is_some());
    }

    #[test]
    fn incomplete_workflow_metadata_blocks_cleanup_conservatively() {
        assert!(referenced_paths("<!-- wisetree:image-attachments: [not-json -->").is_none());
    }

    /// Exactly the four types every supported harness accepts — no more.
    #[test]
    fn recognizes_the_explicit_supported_formats() {
        let formats: &[(&[u8], &str)] = &[
            (PNG, "image/png"),
            (b"\xff\xd8\xff\xd9", "image/jpeg"),
            (b"GIF89a\0\0\0\0\0\0\0;", "image/gif"),
            (b"RIFF\x04\0\0\0WEBP", "image/webp"),
        ];
        for (bytes, mime_type) in formats {
            assert_eq!(identify_image(bytes).unwrap().1, *mime_type);
        }
    }

    /// Codex rejects BMP/TIFF and Claude's vision API accepts none of these,
    /// so they must fail in the textarea rather than mid-run.
    #[test]
    fn rejects_formats_no_harness_can_read() {
        let unsupported: &[&[u8]] = &[
            b"BM\x1a\0\0\0\0\0\0\0\0\0\0\0\x0c\0\0\0\0\0\0\0\0\0\0\0",
            &[
                0, 0, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 32, 0, 0, 0, 0, 0, 22, 0, 0, 0,
            ],
            b"II*\0\x08\0\0\0\0",
            b"MM\0*\0\0\0\x08\0",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        ];
        for bytes in unsupported {
            assert!(matches!(
                identify_image(bytes),
                Err(ImageUploadError::UnsupportedFormat)
            ));
        }
    }

    #[test]
    fn rejects_images_larger_than_the_harness_limit() {
        let storage = ImageStorage::new(tempdir().unwrap().path().join("uploads"));
        // A GIF whose declared structure is valid but whose payload exceeds
        // the per-image limit every harness enforces.
        let mut oversized = b"GIF89a".to_vec();
        oversized.resize(IMAGE_UPLOAD_MAX_BYTES as usize + 1, 0);
        *oversized.last_mut().unwrap() = 0x3b;
        assert!(matches!(
            storage.ingest_bytes(&oversized),
            Err(ImageUploadError::TooLarge { .. })
        ));
        assert!(!storage.root().exists());
    }
}
