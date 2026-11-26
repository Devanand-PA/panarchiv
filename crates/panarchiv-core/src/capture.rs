//! Camera / capture abstraction.
//!
//! The core never talks to hardware directly. A UI supplies captured image
//! paths (from a camera intent, `getUserMedia`, or a gallery picker) and the
//! core imports them like any other file. This keeps Android camera code in
//! the frontend/plugin layer while the archiver logic stays portable.

use std::path::PathBuf;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub max_images: usize,
    pub preferred_language: String,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            max_images: 10,
            preferred_language: "eng".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CapturedImage {
    pub path: PathBuf,
    pub mime: String,
}

/// Implemented by platform frontends that can drive a camera or gallery.
pub trait CaptureSource {
    fn name(&self) -> &'static str;
    fn capture(&self, options: &CaptureOptions) -> Result<Vec<CapturedImage>>;
}

/// A source backed by files the user already picked (gallery/file picker).
pub struct FileCapture {
    pub paths: Vec<PathBuf>,
}

impl CaptureSource for FileCapture {
    fn name(&self) -> &'static str {
        "files"
    }

    fn capture(&self, options: &CaptureOptions) -> Result<Vec<CapturedImage>> {
        Ok(self
            .paths
            .iter()
            .take(options.max_images)
            .map(|p| CapturedImage {
                path: p.clone(),
                mime: crate::model::mime_for_filename(
                    &p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
                .to_string(),
            })
            .collect())
    }
}
