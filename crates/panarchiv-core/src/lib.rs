//! # panarchiv-core
//!
//! Cross-platform document archiver core: import files/folders (and images
//! captured by a camera), attach optional OCR text, store everything in a
//! content-addressed archive, and share documents peer-to-peer over a LAN or
//! as offline bundles.

pub mod archive;
pub mod bundle;
pub mod capture;
pub mod crypto;
pub mod error;
pub mod lock;
pub mod model;
pub mod ocr;
pub mod p2p;
pub mod paths;
pub mod sync;

pub use archive::{AddOptions, Archive, ItemInfo};
pub use error::{ArchiveError, Result};
pub use model::{ArchiveItem, Document, ItemKind, Manifest, OcrResult};
