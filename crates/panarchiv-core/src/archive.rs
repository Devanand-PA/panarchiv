//! Filesystem-backed archive with a content-addressed blob store.
//!
//! Layout:
//! ```text
//! <root>/
//!   manifest.json          # documents + item metadata (encrypted)
//!   key.bin                # archive encryption key (0600)
//!   blobs/<sha256-hex>     # encrypted bytes of every item
//!   thumbs/<sha256-hex>.png
//! ```
//!
//! Storing by hash means duplicates across imports are free and makes
//! integrity verification trivial (the same hash is used by P2P and bundles).
//! The hash always covers the *plaintext*, so two devices can compare
//! inventories without decrypting anything.
//!
//! # Concurrency
//!
//! The same archive may be open in the desktop app, in the CLI and in the
//! P2P server thread at the same time. Every mutation therefore runs under
//! an exclusive lock and re-reads the manifest before applying the change,
//! so a writer can never overwrite somebody else's update with a stale
//! snapshot. The manifest itself is written to a temporary file and moved
//! into place, so a crash mid-write cannot corrupt it.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{msg, ArchiveError, Result};
use crate::crypto::{Crypto, KeyError};
use crate::lock::lock_archive;
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
    crypto: Crypto,
}

impl Archive {
    /// Open an archive at `root`, creating the directory structure, the
    /// encryption key and an empty manifest if necessary.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_passphrase(root, None)
    }

    /// Open an archive, supplying the passphrase if the archive is
    /// protected by one.
    pub fn open_with_passphrase(root: impl AsRef<Path>, passphrase: Option<&str>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("thumbs"))?;
        fs::create_dir_all(root.join("incoming"))?;

        let crypto = Crypto::load_or_create(&root, passphrase).map_err(key_error)?;

        let (manifest, was_plaintext) = Self::read_manifest_file(&root, &crypto)?;
        let mut archive = Self {
            root,
            manifest,
            crypto,
        };

        // An archive written before encryption was introduced still holds
        // plaintext blobs. Encrypt them now, so nothing is ever left in
        // the clear on disk.
        if was_plaintext {
            archive.encrypt_plaintext_files()?;
        }

        Ok(archive)
    }

    /// Re-encrypt every plaintext blob and thumbnail in the archive.
    ///
    /// This runs once, when an archive created by an older version of the
    /// app is opened for the first time.
    fn encrypt_plaintext_files(&mut self) -> Result<()> {
        let mut hashes: HashSet<String> = HashSet::new();
        for doc in &self.manifest.documents {
            for item in &doc.items {
                hashes.insert(item.blob.clone());
            }
        }

        for hash in hashes {
            let path = self.blob_path(&hash);
            if let Ok(raw) = fs::read(&path) {
                if !crate::crypto::is_encrypted(&raw) {
                    let plain = self.crypto.decrypt_or_plain(&raw)?;
                    self.write_blob(&hash, &plain)?;
                }
            }
            let thumb = self.thumb_path(&hash);
            if let Ok(raw) = fs::read(&thumb) {
                if !crate::crypto::is_encrypted(&raw) {
                    let plain = self.crypto.decrypt_or_plain(&raw)?;
                    self.write_thumb(&hash, &plain)?;
                }
            }
        }

        self.write_manifest()
    }

    /// Read and decrypt the manifest, recovering from an interrupted save if
    /// the manifest is missing but a temporary copy survives.
    ///
    /// The second element of the tuple is true when the manifest was found
    /// in plaintext — that is, the archive predates encryption.
    fn read_manifest_file(root: &Path, crypto: &Crypto) -> Result<(Manifest, bool)> {
        let manifest_path = root.join("manifest.json");
        if manifest_path.exists() {
            let raw = fs::read(&manifest_path)?;
            let was_plaintext = !crate::crypto::is_encrypted(&raw);
            let plain = crypto.decrypt_or_plain(&raw)?;
            return Ok((serde_json::from_slice(&plain)?, was_plaintext));
        }

        // A save may have been interrupted between writing the temporary
        // file and moving it into place. Recover the newest copy.
        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("manifest.json.tmp") {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(modified) = meta.modified() {
                            candidates.push((modified, entry.path()));
                        }
                    }
                }
            }
        }
        candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        for (_, path) in candidates {
            if let Ok(raw) = fs::read(&path) {
                let was_plaintext = !crate::crypto::is_encrypted(&raw);
                if let Ok(plain) = crypto.decrypt_or_plain(&raw) {
                    if let Ok(manifest) = serde_json::from_slice::<Manifest>(&plain) {
                        let _ = fs::rename(&path, root.join("manifest.json"));
                        return Ok((manifest, was_plaintext));
                    }
                }
            }
        }

        let m = Manifest::new("panarchiv archive");
        let data = serde_json::to_string_pretty(&m)?;
        let encrypted = crypto.encrypt(data.as_bytes())?;
        let _ = fs::write(root.join("manifest.json"), encrypted);
        Ok((m, false))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// True when the archive's master key is protected by a passphrase.
    pub fn is_passphrase_protected(&self) -> bool {
        self.crypto.is_wrapped()
    }

    /// Run a mutation against the manifest.
    ///
    /// The archive lock is held for the whole operation and the manifest is
    /// re-read first, so concurrent writers in other processes cannot lose
    /// each other's changes.
    fn mutate<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Manifest) -> Result<()>,
    {
        let _lock = lock_archive(&self.root)?;
        self.reload()?;
        f(&mut self.manifest)?;
        self.write_manifest()
    }

    /// Re-read the manifest from disk, discarding the in-memory copy.
    fn reload(&mut self) -> Result<()> {
        self.manifest = Self::read_manifest_file(&self.root, &self.crypto)?.0;
        Ok(())
    }

    /// Persist the in-memory manifest.
    ///
    /// Prefer the mutating helpers, which take the lock and reload first.
    pub fn save(&self) -> Result<()> {
        let _lock = lock_archive(&self.root)?;
        self.write_manifest()
    }

    /// Write the manifest atomically and encrypted.
    fn write_manifest(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&self.manifest)?;
        let encrypted = self.crypto.encrypt(data.as_bytes())?;
        let final_path = self.root.join("manifest.json");
        let tmp = self.root.join(format!("manifest.json.tmp.{}", uuid::Uuid::new_v4()));
        fs::write(&tmp, &encrypted)?;
        match fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(()),
            Err(first) => {
                // Windows refuses to rename over an existing file.
                if final_path.exists() {
                    let _ = fs::remove_file(&final_path);
                    if fs::rename(&tmp, &final_path).is_ok() {
                        return Ok(());
                    }
                }
                // Leave the temporary file behind: it holds the only good
                // copy of the data and `read_manifest_file` can recover it.
                Err(first.into())
            }
        }
    }

    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(hash)
    }

    pub fn thumb_path(&self, hash: &str) -> PathBuf {
        self.root.join("thumbs").join(format!("{hash}.png"))
    }

    /// Read and decrypt the bytes of a stored blob.
    pub fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        let raw = fs::read(self.blob_path(hash))?;
        self.crypto.decrypt_or_plain(&raw)
    }

    /// Encrypt and store a blob. The hash always refers to the plaintext.
    pub fn write_blob(&self, hash: &str, plaintext: &[u8]) -> Result<()> {
        let encrypted = self.crypto.encrypt(plaintext)?;
        write_atomic(&self.blob_path(hash), &encrypted)
    }

    /// Read and decrypt a stored thumbnail.
    pub fn read_thumb(&self, hash: &str) -> Result<Vec<u8>> {
        let raw = fs::read(self.thumb_path(hash))?;
        self.crypto.decrypt_or_plain(&raw)
    }

    /// Copy `path` into the blob store and return `(hash, byte_count)`.
    ///
    /// The hash covers the plaintext so that inventories can be compared
    /// across devices without decrypting anything.
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
            let plaintext = fs::read(path)?;
            self.write_blob(&hash, &plaintext)?;
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

        let committed = documents.clone();
        self.mutate(|manifest| {
            for doc in &committed {
                manifest.documents.push(doc.clone());
            }
            Ok(())
        })?;
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
            version: source_version(path),
        })
    }

    fn generate_thumbnail(&self, path: &Path, hash: &str) {
        if self.thumb_path(hash).exists() {
            return;
        }
        if let Ok(img) = image::open(path) {
            let thumb = img.thumbnail(320, 320);
            let mut buf: Vec<u8> = Vec::new();
            if thumb
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                .is_ok()
            {
                // The thumbnail is derived from an image the user owns, so
                // it gets the same protection as the blob itself.
                let _ = self.write_thumb(hash, &buf);
            }
        }
    }

    /// Encrypt and store a thumbnail.
    pub fn write_thumb(&self, hash: &str, png: &[u8]) -> Result<()> {
        let encrypted = self.crypto.encrypt(png)?;
        write_atomic(&self.thumb_path(hash), &encrypted)
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
        self.mutate(|manifest| {
            let doc = manifest
                .documents
                .iter_mut()
                .find(|d| d.id == doc_id)
                .ok_or_else(|| ArchiveError::DocumentNotFound(doc_id.to_string()))?;
            let item = doc
                .items
                .iter_mut()
                .find(|i| i.id == item_id)
                .ok_or_else(|| ArchiveError::ItemNotFound(item_id.to_string()))?;
            item.ocr = Some(ocr);
            doc.updated_at = Utc::now();
            Ok(())
        })
    }

    /// Replace the tag list of a document. Blank tags are dropped and
    /// duplicates are collapsed, preserving the order they were given in.
    pub fn set_tags(&mut self, doc_id: &str, tags: Vec<String>) -> Result<()> {
        let mut cleaned: Vec<String> = Vec::new();
        for tag in tags {
            let tag = tag.trim().to_string();
            if !tag.is_empty() && !cleaned.contains(&tag) {
                cleaned.push(tag);
            }
        }
        self.mutate(|manifest| {
            let doc = manifest
                .documents
                .iter_mut()
                .find(|d| d.id == doc_id)
                .ok_or_else(|| ArchiveError::DocumentNotFound(doc_id.to_string()))?;
            doc.tags = cleaned;
            doc.updated_at = Utc::now();
            Ok(())
        })
    }

    pub fn remove_document(&mut self, id: &str) -> Result<()> {
        self.mutate(|manifest| {
            let before = manifest.documents.len();
            manifest.documents.retain(|d| d.id != id);
            if manifest.documents.len() == before {
                return Err(ArchiveError::DocumentNotFound(id.to_string()));
            }
            Ok(())
        })
    }

    /// Copy an item's blob to `dest`, decrypted.
    pub fn export_item(&self, doc_id: &str, item_id: &str, dest: &Path) -> Result<()> {
        let doc = self
            .get(doc_id)
            .ok_or_else(|| ArchiveError::DocumentNotFound(doc_id.to_string()))?;
        let item = doc
            .items
            .iter()
            .find(|i| i.id == item_id)
            .ok_or_else(|| ArchiveError::ItemNotFound(item_id.to_string()))?;
        let bytes = self.read_blob(&item.blob)?;
        fs::write(dest, bytes)?;
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
        let bytes = fs::read(path)?;
        let version = source_version(path);
        self.import_bytes(&bytes, filename, title, tags, version)
    }

    /// Import raw bytes as a new document.
    ///
    /// Used by the network receivers, which already hold the bytes and the
    /// peer's metadata rather than a file on disk.
    pub fn import_bytes(
        &mut self,
        bytes: &[u8],
        filename: &str,
        title: Option<String>,
        tags: Vec<String>,
        version: Option<DateTime<Utc>>,
    ) -> Result<String> {
        let hash = sha256_hex(bytes);
        if !self.blob_path(&hash).exists() {
            self.write_blob(&hash, bytes)?;
        }

        let kind = kind_for_filename(filename);
        let item = ArchiveItem {
            id: uuid::Uuid::new_v4().to_string(),
            blob: hash,
            filename: filename.to_string(),
            mime: mime_for_filename(filename).to_string(),
            kind,
            size: bytes.len() as u64,
            width: None,
            height: None,
            ocr: None,
            version,
        };

        let mut doc = Document::new(
            title.unwrap_or_else(|| filename_without_ext(filename).to_string()),
            tags,
        );
        doc.items.push(item);

        let id = doc.id.clone();
        self.mutate(|manifest| {
            manifest.documents.push(doc);
            Ok(())
        })?;
        Ok(id)
    }

    /// Import raw bytes, replacing an older version of the same file.
    ///
    /// This is what a sync uses when *“select newest”* is on: the device
    /// that was missing the newer version receives it and drops the stale
    /// copy, instead of ending up with two documents of the same name.
    ///
    /// Returns the id of the new document, or `None` when the local copy
    /// is already the newer one and nothing was imported.
    pub fn import_bytes_newest(
        &mut self,
        bytes: &[u8],
        filename: &str,
        title: Option<String>,
        tags: Vec<String>,
        version: Option<DateTime<Utc>>,
    ) -> Result<Option<String>> {
        let hash = sha256_hex(bytes);

        // Look for an item with the same filename — that is the same
        // logical file in a different version.
        let existing: Option<(String, ArchiveItem)> = self
            .manifest
            .documents
            .iter()
            .flat_map(|d| d.items.iter().map(move |i| (d.id.clone(), i.clone())))
            .find(|(_, item)| item.filename == filename);

        if let Some((doc_id, item)) = existing {
            if item.blob == hash {
                // The very same file — nothing to do.
                return Ok(None);
            }
            let incoming_wins = match (item.version, version) {
                (Some(old), Some(new)) => new > old,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                // Neither side knows the version: the smaller hash wins,
                // which is the same deterministic rule the sync plan uses.
                (None, None) => hash < item.blob,
            };
            if !incoming_wins {
                return Ok(None);
            }
            self.remove_item(&doc_id, &item.id)?;
        }

        let id = self.import_bytes(bytes, filename, title, tags, version)?;
        Ok(Some(id))
    }

    /// Remove a single item. The document is removed as well when it was
    /// the item's last one.
    pub fn remove_item(&mut self, doc_id: &str, item_id: &str) -> Result<()> {
        self.mutate(|manifest| {
            let doc = manifest
                .documents
                .iter_mut()
                .find(|d| d.id == doc_id)
                .ok_or_else(|| ArchiveError::DocumentNotFound(doc_id.to_string()))?;
            let before = doc.items.len();
            doc.items.retain(|i| i.id != item_id);
            if doc.items.len() == before {
                return Err(ArchiveError::ItemNotFound(item_id.to_string()));
            }
            doc.updated_at = Utc::now();
            // A document without items is empty; drop it.
            manifest.documents.retain(|d| !d.items.is_empty());
            Ok(())
        })
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
        self.mutate(|manifest| {
            manifest.documents.push(new_doc);
            Ok(())
        })?;
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
        // The hash covers the plaintext, so the blob has to be decrypted
        // before it can be checked.
        let bytes = self.read_blob(hash)?;
        Ok(sha256_hex(&bytes) == hash)
    }

    /// The set of blob hashes this archive holds, with the metadata needed
    /// to compare inventories with another device.
    pub fn inventory(&self) -> Vec<ItemInfo> {
        self.inventory_filtered(None)
    }

    /// The inventory restricted to the documents carrying `tag`.
    ///
    /// This is what a device uses to offer "share this tag" to a peer.
    pub fn inventory_for_tag(&self, tag: &str) -> Vec<ItemInfo> {
        self.inventory_filtered(Some(tag))
    }

    fn inventory_filtered(&self, tag_filter: Option<&str>) -> Vec<ItemInfo> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for doc in &self.manifest.documents {
            if let Some(tag) = tag_filter {
                if !doc.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                    continue;
                }
            }
            for item in &doc.items {
                if !seen.insert(item.blob.clone()) {
                    continue;
                }
                out.push(ItemInfo {
                    hash: item.blob.clone(),
                    filename: item.filename.clone(),
                    size: item.size,
                    version: item.version,
                    title: doc.title.clone(),
                    tags: doc.tags.clone(),
                });
            }
        }
        out
    }

    /// Protect the archive with a passphrase, or remove the protection when
    /// `passphrase` is `None`.
    ///
    /// The master key does not change, so this is instantaneous — the
    /// documents stay exactly as they are, only the key file is rewritten.
    pub fn set_passphrase(&mut self, passphrase: Option<&str>) -> Result<()> {
        self.crypto
            .set_passphrase(&crate::crypto::key_path(&self.root), passphrase)
            .map_err(key_error)?;
        Ok(())
    }

    /// Every tag used in the archive, with the number of documents that
    /// carry it. Sorted by name.
    pub fn tags(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for doc in &self.manifest.documents {
            for tag in &doc.tags {
                *counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }
        counts.into_iter().collect()
    }
}

/// A single entry in an archive inventory, used to compare two devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemInfo {
    /// SHA-256 of the plaintext.
    pub hash: String,
    pub filename: String,
    pub size: u64,
    /// When the source file was last modified, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<DateTime<Utc>>,
    pub title: String,
    pub tags: Vec<String>,
}

/// The modification time of a source file, if the platform provides one.
fn source_version(path: &Path) -> Option<DateTime<Utc>> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    DateTime::<Utc>::from(modified).into()
}

/// Write a file atomically: write to a temporary name, then move it into
/// place so a crash cannot leave a half-written file behind.
fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        uuid::Uuid::new_v4()
    ));
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(first) => {
            if dest.exists() {
                let _ = fs::remove_file(dest);
                if fs::rename(&tmp, dest).is_ok() {
                    return Ok(());
                }
            }
            let _ = fs::remove_file(&tmp);
            Err(first.into())
        }
    }
}

/// Convert a key error into a user-facing archive error.
fn key_error(e: KeyError) -> ArchiveError {
    match e {
        KeyError::PassphraseRequired => ArchiveError::PassphraseRequired,
        KeyError::WrongPassphrase => ArchiveError::WrongPassphrase,
        KeyError::Corrupt => ArchiveError::Message("the archive key file is corrupt".to_string()),
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

    #[test]
    fn set_tags_replaces_cleans_and_persists() {
        let dir = std::env::temp_dir().join(format!("pa-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        fs::write(&file, "hello").unwrap();

        let mut archive = Archive::open(dir.join("archive")).unwrap();
        let docs = archive.add_files(&[file], &AddOptions::default()).unwrap();
        let id = docs[0].id.clone();

        // Blank tags are dropped, duplicates collapsed, order preserved.
        archive
            .set_tags(
                &id,
                vec![
                    "finance".into(),
                    "  ".into(),
                    " receipts ".into(),
                    "finance".into(),
                ],
            )
            .unwrap();
        assert_eq!(archive.get(&id).unwrap().tags, vec!["finance", "receipts"]);

        // The change survives a reopen (i.e. it hit the manifest).
        let reopened = Archive::open(dir.join("archive")).unwrap();
        assert_eq!(reopened.get(&id).unwrap().tags, vec!["finance", "receipts"]);

        // Tags can be cleared entirely.
        archive.set_tags(&id, vec![]).unwrap();
        assert!(archive.get(&id).unwrap().tags.is_empty());

        // Unknown documents report an error instead of silently succeeding.
        assert!(archive.set_tags("nope", vec!["x".into()]).is_err());
    }

    /// A tag change must not be lost when another writer saves the archive
    /// in between. This was the bug that made tags "not work".
    #[test]
    fn concurrent_writers_do_not_lose_tag_changes() {
        let dir = std::env::temp_dir().join(format!("pa-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("archive");
        let file = dir.join("notes.txt");
        fs::write(&file, "hello").unwrap();

        let mut seed = Archive::open(&root).unwrap();
        let docs = seed.add_files(&[file], &AddOptions::default()).unwrap();
        let id = docs[0].id.clone();
        drop(seed);

        // Two independent handles on the same root, exactly like the GUI
        // and the P2P server thread would have.
        let mut tagger = Archive::open(&root).unwrap();
        let mut importer = Archive::open(&root).unwrap();

        tagger.set_tags(&id, vec!["finance".into()]).unwrap();

        let other = dir.join("other.txt");
        fs::write(&other, "other").unwrap();
        importer.add_files(&[other], &AddOptions::default()).unwrap();

        let reopened = Archive::open(&root).unwrap();
        assert_eq!(
            reopened.get(&id).unwrap().tags,
            vec!["finance"],
            "the tag change must survive another writer"
        );
        assert_eq!(reopened.list().len(), 2);
    }

    /// An archive written before encryption was introduced is opened,
    /// readable, and rewritten encrypted.
    #[test]
    fn legacy_plaintext_archive_is_encrypted_on_open() {
        let dir = std::env::temp_dir().join(format!("pa-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("archive");
        fs::create_dir_all(root.join("blobs")).unwrap();

        // Write a plaintext blob and a plaintext manifest, the way an
        // older version of the app did.
        let content = b"legacy secret contents";
        let hash = sha256_hex(content);
        fs::write(root.join("blobs").join(&hash), content).unwrap();

        let item = ArchiveItem {
            id: "item-1".into(),
            blob: hash.clone(),
            filename: "legacy.txt".into(),
            mime: "text/plain".into(),
            kind: crate::model::ItemKind::Other,
            size: content.len() as u64,
            width: None,
            height: None,
            ocr: None,
            version: None,
        };
        let mut doc = Document::new("Legacy".to_string(), vec!["old".to_string()]);
        doc.items.push(item);
        let manifest = Manifest {
            version: 1,
            name: "legacy".into(),
            documents: vec![doc],
        };
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // Opening it must succeed and transparently encrypt everything.
        let archive = Archive::open(&root).unwrap();
        assert_eq!(archive.list().len(), 1);
        assert_eq!(archive.get(archive.list()[0].id.as_str()).unwrap().tags, vec!["old"]);
        assert_eq!(archive.read_blob(&hash).unwrap(), content);

        let raw_manifest = fs::read(root.join("manifest.json")).unwrap();
        assert!(
            crate::crypto::is_encrypted(&raw_manifest),
            "the manifest must be encrypted after the upgrade"
        );
        let raw_blob = fs::read(root.join("blobs").join(&hash)).unwrap();
        assert!(
            crate::crypto::is_encrypted(&raw_blob),
            "the blob must be encrypted after the upgrade"
        );
        assert!(!raw_blob.windows(content.len()).any(|w| w == content));
        assert!(archive.verify().is_empty());
    }
}
