//! Filesystem-backed archive with a content-addressed blob store.
//!
//! Layout:
//! ```text
//! <root>/
//!   manifest.json          # documents + item metadata
//!   blobs/<sha256-hex>     # raw bytes of every item
//!   thumbs/<sha256-hex>.png
//! ```
//!
//! Storing by hash means duplicates across imports are free and makes
//! integrity verification trivial (the same hash is used by P2P and bundles).

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::error::{msg, ArchiveError, Result};
use crate::model::{
    kind_for_filename, mime_for_filename, ArchiveItem, Document, Manifest, OcrResult,
};

/// Options controlling how paths are imported into the archive.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// When provided, all imported files share this document title.
    pub title: Option<String>,
    pub tags: Vec<String>,
    /// If true, all files become items of one document instead of one
    /// document per file. Useful for a multi-page scan.
    pub group: bool,
}

pub struct Archive {
    root: PathBuf,
    manifest: Manifest,
}

impl Archive {
    /// Open an archive at `root`, creating the directory structure and an
    /// empty manifest if necessary.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("thumbs"))?;
        fs::create_dir_all(root.join("incoming"))?;

        let manifest_path = root.join("manifest.json");
        let manifest = if manifest_path.exists() {
            let data = fs::read_to_string(&manifest_path)?;
            serde_json::from_str(&data)?
        } else {
            let m = Manifest::new("panarchiv archive");
            fs::write(&manifest_path, serde_json::to_string_pretty(&m)?)?;
            m
        };

        Ok(Self { root, manifest })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Persist the in-memory manifest atomically.
    pub fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&self.manifest)?;
        let tmp = self.root.join("manifest.json.tmp");
        fs::write(&tmp, data)?;
        let final_path = self.root.join("manifest.json");
        // Windows cannot rename over an existing file.
        if final_path.exists() {
            fs::remove_file(&final_path)?;
        }
        fs::rename(&tmp, &final_path)?;
        Ok(())
    }

    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(hash)
    }

    pub fn thumb_path(&self, hash: &str) -> PathBuf {
        self.root.join("thumbs").join(format!("{hash}.png"))
    }

    /// Copy `path` into the blob store and return `(hash, byte_count)`.
    pub fn store_blob(&self, path: &Path) -> Result<(String, u64)> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as u64;
        }
        let hash = hex(hasher.finalize().as_slice());
        let dest = self.blob_path(&hash);
        if !dest.exists() {
            let tmp = self.root.join("blobs").join(format!("{hash}.tmp"));
            fs::copy(path, &tmp)?;
            fs::rename(&tmp, &dest)?;
        }
        Ok((hash, total))
    }

    /// Import one or more files. Returns the created documents.
    pub fn add_files(&mut self, paths: &[PathBuf], opts: &AddOptions) -> Result<Vec<Document>> {
        if paths.is_empty() {
            return Err(msg("no files to import"));
        }

        let mut documents = Vec::new();
        if opts.group {
            documents.push(Document::new(
                opts.title
                    .clone()
                    .unwrap_or_else(|| "Imported scan".to_string()),
                opts.tags.clone(),
            ));
        }

        for path in paths {
            if !path.is_file() {
                continue;
            }
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unnamed".to_string());

            if opts.group {
                let item = self.make_item(path, &filename)?;
                documents[0].items.push(item);
            } else {
                let title = opts
                    .title
                    .clone()
                    .unwrap_or_else(|| filename_without_ext(&filename).to_string());
                let mut doc = Document::new(title, opts.tags.clone());
                let item = self.make_item(path, &filename)?;
                doc.items.push(item);
                documents.push(doc);
            }
        }

        if documents.iter().all(|d| d.items.is_empty()) {
            return Err(msg("none of the given paths were files"));
        }

        for doc in documents.iter() {
            self.manifest.documents.push(doc.clone());
        }
        self.save()?;
        Ok(documents)
    }

    fn make_item(&self, path: &Path, filename: &str) -> Result<ArchiveItem> {
        let (hash, size) = self.store_blob(path)?;
        let kind = kind_for_filename(filename);
        let (width, height) = if kind == crate::model::ItemKind::Image {
            match image::image_dimensions(path) {
                Ok((w, h)) => {
                    self.generate_thumbnail(path, &hash);
                    (Some(w), Some(h))
                }
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };

        Ok(ArchiveItem {
            id: uuid::Uuid::new_v4().to_string(),
            blob: hash,
            filename: filename.to_string(),
            mime: mime_for_filename(filename).to_string(),
            kind,
            size,
            width,
            height,
            ocr: None,
        })
    }

    fn generate_thumbnail(&self, path: &Path, hash: &str) {
        let dest = self.thumb_path(hash);
        if dest.exists() {
            return;
        }
        if let Ok(img) = image::open(path) {
            let thumb = img.thumbnail(320, 320);
            let _ = thumb.save(dest);
        }
    }

    /// Recursively import all files under `dir`. One document per file.
    pub fn add_folder(
        &mut self,
        dir: &Path,
        recursive: bool,
        tags: Vec<String>,
    ) -> Result<Vec<Document>> {
        if !dir.is_dir() {
            return Err(msg(format!("not a directory: {}", dir.display())));
        }
        let mut paths = Vec::new();
        let walker = walkdir::WalkDir::new(dir).max_depth(if recursive { usize::MAX } else { 1 });
        for entry in walker {
            let entry = entry.map_err(|e| msg(format!("walk error: {e}")))?;
            if entry.file_type().is_file() {
                paths.push(entry.path().to_path_buf());
            }
        }
        self.add_files(
            &paths,
            &AddOptions {
                title: None,
                tags,
                group: false,
            },
        )
    }

    pub fn list(&self) -> &[Document] {
        &self.manifest.documents
    }

    pub fn get(&self, id: &str) -> Option<&Document> {
        self.manifest.documents.iter().find(|d| d.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Document> {
        self.manifest.documents.iter_mut().find(|d| d.id == id)
    }

    pub fn search(&self, needle: &str) -> Vec<&Document> {
        self.manifest
            .documents
            .iter()
            .filter(|d| d.matches(needle))
            .collect()
    }

    /// Attach OCR text to a single item.
    pub fn set_ocr(&mut self, doc_id: &str, item_id: &str, ocr: OcrResult) -> Result<()> {
        let doc = self
            .get_mut(doc_id)
            .ok_or_else(|| ArchiveError::DocumentNotFound(doc_id.to_string()))?;
        let item = doc
            .items
            .iter_mut()
            .find(|i| i.id == item_id)
            .ok_or_else(|| ArchiveError::ItemNotFound(item_id.to_string()))?;
        item.ocr = Some(ocr);
        doc.updated_at = Utc::now();
        self.save()
    }

    pub fn remove_document(&mut self, id: &str) -> Result<()> {
        let before = self.manifest.documents.len();
        self.manifest.documents.retain(|d| d.id != id);
        if self.manifest.documents.len() == before {
            return Err(ArchiveError::DocumentNotFound(id.to_string()));
        }
        self.save()
    }

    /// Copy an item's blob to `dest`.
    pub fn export_item(&self, doc_id: &str, item_id: &str, dest: &Path) -> Result<()> {
        let doc = self
            .get(doc_id)
            .ok_or_else(|| ArchiveError::DocumentNotFound(doc_id.to_string()))?;
        let item = doc
            .items
            .iter()
            .find(|i| i.id == item_id)
            .ok_or_else(|| ArchiveError::ItemNotFound(item_id.to_string()))?;
        fs::copy(self.blob_path(&item.blob), dest)?;
        Ok(())
    }

    /// Import an arbitrary file (used by P2P receiver and bundle import).
    /// Returns the new document id.
    pub fn import_file(
        &mut self,
        path: &Path,
        filename: &str,
        title: Option<String>,
        tags: Vec<String>,
    ) -> Result<String> {
        let mut docs = self.add_files(
            &[path.to_path_buf()],
            &AddOptions {
                title: Some(title.unwrap_or_else(|| filename_without_ext(filename).to_string())),
                tags,
                group: false,
            },
        )?;
        // `add_files` sets filename from the source path; overwrite with the
        // logical filename supplied by the peer.
        let doc = &mut docs[0];
        if let Some(item) = doc.items.first_mut() {
            item.filename = filename.to_string();
            let kind = kind_for_filename(filename);
            item.kind = kind;
            item.mime = mime_for_filename(filename).to_string();
            if kind == crate::model::ItemKind::Image {
                let (w, h) = image::image_dimensions(&self.blob_path(&item.blob)).unwrap_or((0, 0));
                item.width = if w == 0 { None } else { Some(w) };
                item.height = if h == 0 { None } else { Some(h) };
            }
        }
        // The clone pushed into the manifest happened inside add_files, so
        // patch the stored copy as well.
        if let Some(stored) = self.get_mut(&doc.id) {
            stored.items = doc.items.clone();
            self.save()?;
        }
        Ok(doc.id.clone())
    }

    /// Merge a document from a bundle, assigning a fresh id to avoid
    /// collisions with locally-created documents.
    pub fn merge_document(&mut self, doc: &Document) -> Result<String> {
        let mut new_doc = doc.clone();
        new_doc.id = uuid::Uuid::new_v4().to_string();
        new_doc.created_at = Utc::now();
        new_doc.updated_at = Utc::now();
        for item in &mut new_doc.items {
            item.id = uuid::Uuid::new_v4().to_string();
        }
        let id = new_doc.id.clone();
        self.manifest.documents.push(new_doc);
        self.save()?;
        Ok(id)
    }

    /// Verify that every blob referenced by the manifest still matches its
    /// hash. Returns the list of broken hashes.
    pub fn verify(&self) -> Vec<String> {
        let mut checked = HashSet::new();
        let mut broken = Vec::new();
        for doc in &self.manifest.documents {
            for item in &doc.items {
                if !checked.insert(item.blob.clone()) {
                    continue;
                }
                match self.verify_blob(&item.blob) {
                    Ok(true) => {}
                    _ => broken.push(item.blob.clone()),
                }
            }
        }
        broken
    }

    fn verify_blob(&self, hash: &str) -> Result<bool> {
        let (actual, _) = self.store_blob(&self.blob_path(hash))?;
        Ok(actual == hash)
    }
}

fn filename_without_ext(name: &str) -> &str {
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

/// Ensure `dest` has a unique name by appending a numeric suffix.
pub fn unique_path(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let ext = dest.extension().map(|e| e.to_string_lossy().into_owned());
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    for i in 1..10_000 {
        let name = match &ext {
            Some(e) => format!("{stem}_{i}.{e}"),
            None => format!("{stem}_{i}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}_overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_and_search_works() {
        let dir = std::env::temp_dir().join(format!("pa-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("receipt.txt");
        fs::write(&file, "hello receipt").unwrap();

        let mut archive = Archive::open(dir.join("archive")).unwrap();
        let docs = archive.add_files(&[file], &AddOptions::default()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(archive.list().len(), 1);
        assert_eq!(archive.search("receipt").len(), 1);
        assert!(archive.verify().is_empty());
    }

    #[test]
    fn grouping_creates_one_document() {
        let dir = std::env::temp_dir().join(format!("pa-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();

        let mut archive = Archive::open(dir.join("archive")).unwrap();
        let docs = archive
            .add_files(
                &[a, b],
                &AddOptions {
                    title: Some("Scans".into()),
                    tags: vec![],
                    group: true,
                },
            )
            .unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].items.len(), 2);
    }
}
