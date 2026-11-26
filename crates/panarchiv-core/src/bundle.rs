//! Offline archive bundles for "sneakernet" transfer (USB, email, cloud).
//!
//! A `.parbundle` is a gzip-compressed tar containing a `bundle.json`
//! manifest and the exact blobs referenced by its documents.
//!
//! # Encryption
//!
//! Bundles can optionally be encrypted with a passphrase. The recipient
//! needs the same passphrase to open them, so the passphrase has to travel
//! over a different channel than the bundle itself. A bundle without a
//! passphrase stays a plain gzip tar for backwards compatibility.
//!
//! ```text
//! plaintext bundle:  gzip(tar)
//! encrypted bundle:  "PARBUNDLE2" | flags=1 | salt | costs | nonce | ciphertext
//! ```
//!
//! The blobs inside a bundle are the *plaintext* documents: the archive
//! decrypts them on export and re-encrypts them on import, so a bundle is
//! a portable, self-contained copy of the documents it carries.

use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use tar::{Builder, Header};

use crate::archive::{sha256_hex, Archive};
use crate::crypto;
use crate::error::{msg, ArchiveError, Result};
use crate::model::Document;

const BUNDLE_VERSION: u32 = 1;

/// Header of an encrypted bundle.
const ENCRYPTED_BUNDLE_MAGIC: &[u8; 11] = b"PARBUNDLE2\0";

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
///
/// `passphrase` encrypts the bundle; the recipient needs the same
/// passphrase to import it.
pub fn export_bundle(
    archive: &Archive,
    doc_ids: Option<&[String]>,
    dest: &Path,
    passphrase: Option<&str>,
) -> Result<usize> {
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

    // Build the tar.gz in memory so it can be encrypted as one blob.
    let mut raw: Vec<u8> = Vec::new();
    {
        let encoder = GzEncoder::new(&mut raw, Compression::default());
        let mut builder = Builder::new(encoder);
        append_bytes(&mut builder, "bundle.json", &json)?;

        let mut hashed = std::collections::HashSet::new();
        for doc in &manifest.documents {
            for item in &doc.items {
                if !hashed.insert(item.blob.clone()) {
                    continue;
                }
                // Decrypt on the way out: a bundle is a portable copy, not
                // a piece of the encrypted archive.
                let bytes = archive.read_blob(&item.blob)?;
                append_bytes(&mut builder, &format!("blobs/{}", item.blob), &bytes)?;
            }
        }
        builder.finish()?;
    }

    let payload = match passphrase {
        Some(p) if !p.is_empty() => {
            let mut out = Vec::with_capacity(ENCRYPTED_BUNDLE_MAGIC.len() + raw.len());
            out.extend_from_slice(ENCRYPTED_BUNDLE_MAGIC);
            out.push(1);
            out.extend_from_slice(&crypto::encrypt_with_passphrase(&raw, p)?);
            out
        }
        _ => raw,
    };

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = File::create(dest)?;
    file.write_all(&payload)?;
    file.flush()?;
    Ok(manifest.documents.len())
}

/// Export every document carrying `tag` as one bundle.
///
/// This is how a user shares an *entire tag* of documents: the recipient
/// gets every document with that tag, with its title and its other tags
/// intact.
pub fn export_bundle_by_tag(
    archive: &Archive,
    tag: &str,
    dest: &Path,
    passphrase: Option<&str>,
) -> Result<usize> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Err(msg("no tag given"));
    }
    let ids: Vec<String> = archive
        .list()
        .iter()
        .filter(|d| d.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)))
        .map(|d| d.id.clone())
        .collect();
    if ids.is_empty() {
        return Err(msg(format!("no documents tagged '{tag}'")));
    }
    export_bundle(archive, Some(&ids), dest, passphrase)
}

/// Import a bundle into an archive. Blobs are deduplicated by hash and
/// documents receive fresh ids to avoid collisions.
pub fn import_bundle(
    archive: &mut Archive,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<Vec<String>> {
    let mut raw = std::fs::read(path)?;
    if raw.starts_with(ENCRYPTED_BUNDLE_MAGIC) {
        let flags = *raw.get(ENCRYPTED_BUNDLE_MAGIC.len()).unwrap_or(&0);
        if flags != 1 {
            return Err(msg("unsupported encrypted bundle format"));
        }
        let body = &raw[ENCRYPTED_BUNDLE_MAGIC.len() + 1..];
        let passphrase = passphrase
            .filter(|p| !p.is_empty())
            .ok_or(ArchiveError::PassphraseRequired)?;
        raw = crypto::decrypt_with_passphrase(body, passphrase)?;
    }

    let decoder = GzDecoder::new(Cursor::new(raw));
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
        // Re-encrypt on the way in: the archive only ever stores ciphertext.
        if !archive.blob_path(&hash).exists() {
            archive.write_blob(&hash, &data)?;
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

    fn tmpdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pa-bundle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bundle_round_trip() {
        let dir = tmpdir();
        let file = dir.join("note.txt");
        std::fs::write(&file, "bundle test content").unwrap();

        let mut a = Archive::open(dir.join("a")).unwrap();
        a.add_files(&[file], &AddOptions::default()).unwrap();
        let bundle = dir.join("out.parbundle");
        assert_eq!(export_bundle(&a, None, &bundle, None).unwrap(), 1);

        let mut b = Archive::open(dir.join("b")).unwrap();
        let ids = import_bundle(&mut b, &bundle, None).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(b.list().len(), 1);
        assert!(b.verify().is_empty());
    }

    #[test]
    fn archive_blobs_are_encrypted_on_disk() {
        let dir = tmpdir();
        let file = dir.join("secret.txt");
        std::fs::write(&file, "top secret contents").unwrap();

        let mut a = Archive::open(dir.join("a")).unwrap();
        let docs = a.add_files(&[file], &AddOptions::default()).unwrap();
        let hash = docs[0].items[0].blob.clone();

        let raw = std::fs::read(a.blob_path(&hash)).unwrap();
        assert!(
            !raw.windows(6).any(|w| w == b"secret"),
            "the plaintext must not be visible in the blob file"
        );
        assert_eq!(a.read_blob(&hash).unwrap(), b"top secret contents");
    }

    #[test]
    fn encrypted_bundle_needs_the_passphrase() {
        let dir = tmpdir();
        let file = dir.join("note.txt");
        std::fs::write(&file, "bundle test content").unwrap();

        let mut a = Archive::open(dir.join("a")).unwrap();
        a.add_files(&[file], &AddOptions::default()).unwrap();
        let bundle = dir.join("secret.parbundle");
        export_bundle(&a, None, &bundle, Some("hunter2")).unwrap();

        let raw = std::fs::read(&bundle).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"secret"));

        let mut b = Archive::open(dir.join("b")).unwrap();
        assert!(matches!(
            import_bundle(&mut b, &bundle, None),
            Err(ArchiveError::PassphraseRequired)
        ));
        assert!(matches!(
            import_bundle(&mut b, &bundle, Some("wrong")),
            Err(ArchiveError::Message(_))
        ));

        let mut c = Archive::open(dir.join("c")).unwrap();
        let ids = import_bundle(&mut c, &bundle, Some("hunter2")).unwrap();
        assert_eq!(ids.len(), 1);
        assert!(c.verify().is_empty());
    }

    #[test]
    fn bundle_by_tag_selects_only_that_tag() {
        let dir = tmpdir();
        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, "aaa").unwrap();
        std::fs::write(&f2, "bbb").unwrap();

        let mut a = Archive::open(dir.join("a")).unwrap();
        a.add_files(
            &[f1],
            &AddOptions {
                tags: vec!["work".into()],
                ..Default::default()
            },
        )
        .unwrap();
        a.add_files(
            &[f2],
            &AddOptions {
                tags: vec!["private".into()],
                ..Default::default()
            },
        )
        .unwrap();

        let bundle = dir.join("tag.parbundle");
        assert_eq!(export_bundle_by_tag(&a, "work", &bundle, None).unwrap(), 1);

        let mut b = Archive::open(dir.join("b")).unwrap();
        import_bundle(&mut b, &bundle, None).unwrap();
        assert_eq!(b.list().len(), 1);
        assert_eq!(b.list()[0].tags, vec!["work".to_string()]);
    }
}
