//! Core domain model for panarchiv.
//!
//! A `Document` is a logical container (a scanned report, a contract, a
//! photo of a receipt, ...). It holds one or more `ArchiveItem`s, each of
//! which is a blob stored in the content-addressed store: an image, another
//! document (PDF/DOCX/...), or any other attachment. Every item may carry
//! an OCR result.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type BlobHash = String;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ItemKind {
    #[default]
    Other,
    Image,
    Document,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrResult {
    pub text: String,
    pub language: String,
    pub engine: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveItem {
    pub id: String,
    pub blob: BlobHash,
    pub filename: String,
    pub mime: String,
    pub kind: ItemKind,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr: Option<OcrResult>,
    /// When the source file was last modified. Two devices can hold
    /// different versions of the same file; the newer `version` wins during
    /// a sync. `None` for items imported without source metadata (older
    /// archives, or content received over the network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub items: Vec<ArchiveItem>,
}

impl Document {
    /// Build a new empty document with fresh identity and timestamps.
    pub fn new(title: impl Into<String>, tags: Vec<String>) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            tags,
            created_at: now,
            updated_at: now,
            items: Vec::new(),
        }
    }

    /// True when any item contains the case-insensitive needle in its
    /// filename or OCR text.
    pub fn matches(&self, needle: &str) -> bool {
        let n = needle.to_lowercase();
        if self.title.to_lowercase().contains(&n)
            || self.tags.iter().any(|t| t.to_lowercase().contains(&n))
        {
            return true;
        }
        self.items.iter().any(|i| {
            i.filename.to_lowercase().contains(&n)
                || i.ocr
                    .as_ref()
                    .map(|o| o.text.to_lowercase().contains(&n))
                    .unwrap_or(false)
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub name: String,
    pub documents: Vec<Document>,
}

impl Manifest {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: 1,
            name: name.into(),
            documents: Vec::new(),
        }
    }
}

/// Guess a MIME type from a filename extension. Kept dependency-free so the
/// core stays light on mobile.
pub fn mime_for_filename(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "heic" | "heif" => "image/heic",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" | "md" => "text/plain",
        "rtf" => "application/rtf",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

pub fn kind_for_filename(name: &str) -> ItemKind {
    let mime = mime_for_filename(name);
    if mime.starts_with("image/") {
        ItemKind::Image
    } else if matches!(
        mime,
        "application/pdf"
            | "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-powerpoint"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/rtf"
            | "text/plain"
    ) {
        ItemKind::Document
    } else {
        ItemKind::Other
    }
}
