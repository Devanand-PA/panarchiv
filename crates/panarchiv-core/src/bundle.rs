//! Offline archive bundles for "sneakernet" transfer (USB, email, cloud).
//!
//! A `.parbundle` is a gzip-compressed tar containing a `bundle.json`
//! manifest and the exact blobs referenced by its documents.

use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use tar::{Builder, Header};

use crate::archive::{sha256_hex, Archive};
use crate::error::{msg, ArchiveError, Result};
use crate::model::Document;

const BUNDLE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct BundleManifest {
    version: u32,
    name: String,
    exported_at: DateTime<Utc>,
    documents: Vec<Document>,
}

fn append_bytes<W: std::io::Write>(
    builder: &mut Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, name, Cursor::new(bytes))?;
    Ok(())
}

/// Export documents to a bundle. `None` exports the whole archive.
pub fn export_bundle(archive: &Archive, doc_ids: Option<&[String]>, dest: &Path) -> Result<usize> {
    let documents: Vec<Document> = match doc_ids {
        Some(ids) => {
            let mut selected = Vec::new();
            for id in ids {
                let doc = archive
                    .get(id)
                    .ok_or_else(|| ArchiveError::DocumentNotFound(id.clone()))?;
                selected.push(doc.clone());
            }
            selected
        }
        None => archive.list().to_vec(),
    };

    if documents.is_empty() {
        return Err(msg("no documents to export"));
    }

    let manifest = BundleManifest {
        version: BUNDLE_VERSION,
        name: archive.manifest().name.clone(),
        exported_at: Utc::now(),
        documents,
    };
    let json = serde_json::to_vec_pretty(&manifest)?;

    let file = File::create(dest)?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(encoder);
    append_bytes(&mut builder, "bundle.json", &json)?;

    let mut hashed = std::collections::HashSet::new();
    for doc in &manifest.documents {
        for item in &doc.items {
            if !hashed.insert(item.blob.clone()) {
                continue;
            }
            let bytes = std::fs::read(archive.blob_path(&item.blob))?;
            append_bytes(&mut builder, &format!("blobs/{}", item.blob), &bytes)?;
        }
    }

    builder.finish()?;
    Ok(manifest.documents.len())
}

/// Import a bundle into an archive. Blobs are deduplicated by hash and
/// documents receive fresh ids to avoid collisions.
pub fn import_bundle(archive: &mut Archive, path: &Path) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let decoder = GzDecoder::new(file);
    let mut archive_reader = tar::Archive::new(decoder);

    let mut manifest_bytes = None;
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive_reader.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if name == "bundle.json" {
            manifest_bytes = Some(data);
        } else if let Some(hash) = name.strip_prefix("blobs/") {
            blobs.push((hash.to_string(), data));
        }
    }

    let bytes = manifest_bytes.ok_or_else(|| msg("bundle has no bundle.json"))?;
    let manifest: BundleManifest = serde_json::from_slice(&bytes)?;
    if manifest.version != BUNDLE_VERSION {
        return Err(msg(format!(
            "unsupported bundle version {}",
            manifest.version
        )));
    }

    for (hash, data) in blobs {
        let actual = sha256_hex(&data);
        if actual != hash {
            return Err(ArchiveError::ChecksumMismatch {
                name: hash.clone(),
                expected: hash.clone(),
                actual,
            });
        }
        let dest = archive.blob_path(&hash);
        if !dest.exists() {
            std::fs::write(dest, data)?;
        }
    }

    let mut ids = Vec::new();
    for doc in &manifest.documents {
        ids.push(archive.merge_document(doc)?);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::AddOptions;

    #[test]
    fn bundle_round_trip() {
        let dir = std::env::temp_dir().join(format!("pa-bundle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "bundle test content").unwrap();

        let mut a = Archive::open(dir.join("a")).unwrap();
        a.add_files(&[file], &AddOptions::default()).unwrap();
        let bundle = dir.join("out.parbundle");
        assert_eq!(export_bundle(&a, None, &bundle).unwrap(), 1);

        let mut b = Archive::open(dir.join("b")).unwrap();
        let ids = import_bundle(&mut b, &bundle).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(b.list().len(), 1);
        assert!(b.verify().is_empty());
    }
}
