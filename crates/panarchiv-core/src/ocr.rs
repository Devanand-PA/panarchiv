//! OCR with a platform-selectable engine.
//!
//! On desktop we shell out to a bundled/installed Tesseract binary. The
//! `OcrEngine` trait is deliberately small so a mobile frontend can supply a
//! native implementation (e.g. ML Kit / Vision) without touching the rest of
//! the core.

use std::path::Path;
use std::process::Command;

use crate::archive::Archive;
use crate::error::{msg, ArchiveError, Result};
use crate::model::OcrResult;

pub trait OcrEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn available(&self) -> bool;
    fn recognize(&self, path: &Path, language: &str) -> Result<String>;
}

/// Tesseract invoked as a subprocess. Works on Windows/macOS/Linux when
/// Tesseract is installed or bundled next to the app.
pub struct TesseractCli {
    pub binary: String,
}

impl Default for TesseractCli {
    fn default() -> Self {
        Self {
            binary: "tesseract".to_string(),
        }
    }
}

impl OcrEngine for TesseractCli {
    fn name(&self) -> &'static str {
        "tesseract"
    }

    fn available(&self) -> bool {
        Command::new(&self.binary)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn recognize(&self, path: &Path, language: &str) -> Result<String> {
        let output = Command::new(&self.binary)
            .arg(path)
            .arg("stdout")
            .arg("-l")
            .arg(language)
            .output()
            .map_err(|e| ArchiveError::Ocr(format!("failed to start tesseract: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ArchiveError::Ocr(format!(
                "tesseract exited with {}: {}",
                output.status, stderr
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Placeholder engine used when no native backend is available. It reports
/// itself unavailable so callers can skip OCR cleanly.
pub struct NoopOcr;

impl OcrEngine for NoopOcr {
    fn name(&self) -> &'static str {
        "none"
    }

    fn available(&self) -> bool {
        false
    }

    fn recognize(&self, _path: &Path, _language: &str) -> Result<String> {
        Err(ArchiveError::Ocr(
            "no OCR engine available on this platform".into(),
        ))
    }
}

/// Pick the best engine available on the current platform.
pub fn default_engine() -> Box<dyn OcrEngine> {
    let cli = TesseractCli::default();
    if cli.available() {
        Box::new(cli)
    } else {
        Box::new(NoopOcr)
    }
}

/// Run OCR on an item and store the result in the archive.
///
/// The blob is copied to a temporary file with the original extension because
/// Tesseract relies on the file extension to pick an image decoder.
pub fn recognize_item(
    archive: &mut Archive,
    doc_id: &str,
    item_id: &str,
    engine: &dyn OcrEngine,
    language: &str,
) -> Result<OcrResult> {
    if !engine.available() {
        return Err(ArchiveError::Ocr(format!(
            "OCR engine '{}' is not available",
            engine.name()
        )));
    }

    let doc = archive
        .get(doc_id)
        .ok_or_else(|| ArchiveError::DocumentNotFound(doc_id.to_string()))?;
    let item = doc
        .items
        .iter()
        .find(|i| i.id == item_id)
        .ok_or_else(|| ArchiveError::ItemNotFound(item_id.to_string()))?;

    if item.kind != crate::model::ItemKind::Image {
        return Err(msg("OCR is only supported on image items"));
    }

    let ext = Path::new(&item.filename)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "png".to_string());
    let tmp = std::env::temp_dir().join(format!("panarchiv-ocr-{}.{}", uuid::Uuid::new_v4(), ext));
    // The blob is encrypted at rest, so it has to be decrypted before the
    // OCR engine can look at it.
    let plaintext = archive.read_blob(&item.blob)?;
    std::fs::write(&tmp, &plaintext)?;

    let text = engine.recognize(&tmp, language);
    let _ = std::fs::remove_file(&tmp);
    let text = text?;

    let result = OcrResult {
        text,
        language: language.to_string(),
        engine: engine.name().to_string(),
        created_at: chrono::Utc::now(),
    };
    archive.set_ocr(doc_id, item_id, result.clone())?;
    Ok(result)
}
