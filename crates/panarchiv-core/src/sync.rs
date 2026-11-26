//! Device-to-device synchronisation.
//!
//! Two devices on the same network can compare their archives and exchange
//! whatever is missing. The exchange is symmetric: each side sends its
//! inventory, both sides compute a plan, and then each sends the files the
//! other side is missing.
//!
//! # Versions
//!
//! Exact duplicates are matched by the SHA-256 of the plaintext, so the
//! same file never travels twice. Two devices can also hold *different*
//! versions of the same logical file (matched by filename). With
//! `prefer_newest` the side holding the newer `version` keeps it and sends
//! it to the other side; with the option off, both versions are kept and
//! both are transferred.
//!
//! When neither side knows a version the hash breaks the tie, so the two
//! devices always agree on who wins and the transfer cannot ping-pong.
//!
//! # Tags
//!
//! The inventory carries each item's document title and tags, so a synced
//! file arrives with the tags it had on the other device.
//!
//! # Sharing a tag
//!
//! A synchronisation can be narrowed to a single tag. In `share_only` mode
//! the sender pushes its tagged documents to the receiver and asks for
//! nothing back — that is how a user shares an *entire tag* of documents
//! with another device.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::archive::{Archive, ItemInfo};
use crate::error::{msg, Result};
use crate::p2p::{read_exact_payload, read_json, write_json, write_payload};

/// Protocol marker for a synchronisation handshake.
pub const SYNC_MAGIC: &str = "PARCHIV2";

/// One file taking part in a synchronisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferItem {
    /// SHA-256 of the plaintext.
    pub hash: String,
    pub filename: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<DateTime<Utc>>,
    /// Title of the document the file belongs to.
    pub title: String,
    /// Tags of that document.
    pub tags: Vec<String>,
}

impl From<&ItemInfo> for TransferItem {
    fn from(item: &ItemInfo) -> Self {
        Self {
            hash: item.hash.clone(),
            filename: item.filename.clone(),
            size: item.size,
            version: item.version,
            title: item.title.clone(),
            tags: item.tags.clone(),
        }
    }
}

/// The client's opening message: who it is and what it holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyncRequest {
    pub magic: String,
    pub sender: String,
    /// Resolve same-named files by keeping the newer version.
    pub prefer_newest: bool,
    /// Carry document tags along with the files.
    pub sync_tags: bool,
    /// Restrict the exchange to documents carrying this tag. `None` means
    /// "every document".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_filter: Option<String>,
    /// Push our documents to the peer without asking for anything back.
    /// This is what "share this tag" uses.
    #[serde(default)]
    pub share_only: bool,
    pub items: Vec<ItemInfo>,
}

/// The server's answer: what each side has to send.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyncPlan {
    pub accept: bool,
    pub message: String,
    /// Files the client must send to the server.
    pub send_to_peer: Vec<TransferItem>,
    /// Files the server must send to the client.
    pub peer_sends: Vec<TransferItem>,
}

/// The final status of a synchronisation.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyncReceipt {
    pub ok: bool,
    pub message: String,
    /// Files the local side received.
    pub received: usize,
    /// Files the local side sent.
    pub sent: usize,
    /// Tags applied to the received files.
    pub tags_received: usize,
}

/// User-visible options for a synchronisation run.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Keep the newer of two versions of the same file and share it with
    /// the other device.
    pub prefer_newest: bool,
    /// Copy document titles and tags along with the files.
    pub sync_tags: bool,
    /// Only sync documents carrying this tag.
    pub tag_filter: Option<String>,
    /// Push to the peer without pulling anything back (share a tag).
    pub share_only: bool,
}

impl SyncOptions {
    /// A plain two-way synchronisation of everything.
    pub fn full_sync() -> Self {
        Self {
            prefer_newest: false,
            sync_tags: true,
            tag_filter: None,
            share_only: false,
        }
    }

    /// Push every document carrying `tag` to the peer.
    pub fn share_tag(tag: impl Into<String>) -> Self {
        Self {
            prefer_newest: false,
            sync_tags: true,
            tag_filter: Some(tag.into()),
            share_only: true,
        }
    }
}

/// What a synchronisation run did.
#[derive(Debug, Clone, Default)]
pub struct SyncSummary {
    pub peer_name: String,
    pub sent: usize,
    pub received: usize,
    pub tags_received: usize,
    pub skipped: usize,
}

/// Work out what each side has to send.
///
/// Returns `(send_to_peer, peer_sends)`: the first list is what the local
/// side must transfer, the second is what the remote side must transfer.
///
/// `tag_filter` narrows both inventories to the documents carrying that
/// tag; `None` means "every document".
pub fn plan(
    local: &[ItemInfo],
    remote: &[ItemInfo],
    prefer_newest: bool,
    tag_filter: Option<&str>,
) -> (Vec<TransferItem>, Vec<TransferItem>) {
    let local: Vec<&ItemInfo> = local
        .iter()
        .filter(|i| matches_tag(i, tag_filter))
        .collect();
    let remote: Vec<&ItemInfo> = remote
        .iter()
        .filter(|i| matches_tag(i, tag_filter))
        .collect();

    let local_hashes: HashSet<&str> = local.iter().map(|i| i.hash.as_str()).collect();
    let remote_hashes: HashSet<&str> = remote.iter().map(|i| i.hash.as_str()).collect();

    // The newest known version of each filename on each side.
    let local_newest = newest_by_filename(&local);
    let remote_newest = newest_by_filename(&remote);

    let mut send_to_peer = Vec::new();
    for item in &local {
        if remote_hashes.contains(item.hash.as_str()) {
            continue; // the peer already has this exact file
        }
        if prefer_newest {
            if let Some(peer_item) = remote_newest.get(item.filename.as_str()) {
                if peer_item.wins_over(item) {
                    continue; // the peer has the newer version; it will send it
                }
            }
        }
        send_to_peer.push(TransferItem::from(*item));
    }

    let mut peer_sends = Vec::new();
    for item in &remote {
        if local_hashes.contains(item.hash.as_str()) {
            continue;
        }
        if prefer_newest {
            if let Some(local_item) = local_newest.get(item.filename.as_str()) {
                if local_item.wins_over(item) {
                    continue; // we have the newer version; it is in send_to_peer
                }
            }
        }
        peer_sends.push(TransferItem::from(*item));
    }

    (send_to_peer, peer_sends)
}

/// True when an item belongs to the requested tag (or no tag was requested).
fn matches_tag(item: &ItemInfo, tag_filter: Option<&str>) -> bool {
    match tag_filter {
        None => true,
        Some(tag) => item.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)),
    }
}

/// Keep the newest item for each filename.
fn newest_by_filename<'a>(items: &'a [&'a ItemInfo]) -> HashMap<&'a str, &'a ItemInfo> {
    let mut out: HashMap<&str, &ItemInfo> = HashMap::new();
    for item in items {
        let entry = out.entry(item.filename.as_str());
        match entry {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(item);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if item.wins_over(slot.get()) {
                    slot.insert(item);
                }
            }
        }
    }
    out
}

impl ItemInfo {
    /// True when this item should win over `other` for the same filename.
    ///
    /// Newer versions win. When the versions are equal or both unknown the
    /// smaller hash wins, which is deterministic and therefore stops two
    /// devices from pushing the same file back and forth forever.
    fn wins_over(&self, other: &ItemInfo) -> bool {
        match (self.version, other.version) {
            (Some(a), Some(b)) if a > b => true,
            (Some(a), Some(b)) if a < b => false,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            _ => self.hash < other.hash,
        }
    }
}

/// Connect to a peer and synchronise with it.
pub fn sync_with_peer(
    addr: std::net::SocketAddr,
    archive: &mut Archive,
    sender_name: &str,
    options: &SyncOptions,
) -> Result<SyncSummary> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;

    let mut writer = BufWriter::new(stream.try_clone()?);
    let mut reader = BufReader::new(stream);

    let request = SyncRequest {
        magic: SYNC_MAGIC.to_string(),
        sender: sender_name.to_string(),
        prefer_newest: options.prefer_newest,
        sync_tags: options.sync_tags,
        tag_filter: options.tag_filter.clone(),
        share_only: options.share_only,
        items: archive.inventory(),
    };
    write_json(&mut writer, &request)?;

    let plan: SyncPlan = read_json(&mut reader)?;
    if !plan.accept {
        return Err(msg(plan.message));
    }

    // 1. Send the files the peer is missing.
    let mut sent = 0usize;
    for item in &plan.send_to_peer {
        let bytes = archive.read_blob(&item.hash)?;
        write_payload(&mut writer, &bytes)?;
        sent += 1;
    }
    writer.flush()?;

    // 2. Receive the files we are missing.
    let mut received = 0usize;
    let mut tags_received = 0usize;
    for item in &plan.peer_sends {
        let len = read_length(&mut reader)?;
        let bytes = read_exact_payload(&mut reader, len)?;
        let tags = if options.sync_tags {
            item.tags.clone()
        } else {
            Vec::new()
        };
        if !tags.is_empty() {
            tags_received += 1;
        }
        // With "select newest" the incoming file replaces an older version
        // of the same file instead of piling up next to it.
        if options.prefer_newest {
            archive.import_bytes_newest(
                &bytes,
                &item.filename,
                Some(item.title.clone()),
                tags,
                item.version,
            )?;
        } else {
            archive.import_bytes(
                &bytes,
                &item.filename,
                Some(item.title.clone()),
                tags,
                item.version,
            )?;
        }
        received += 1;
    }

    let _receipt: SyncReceipt = read_json(&mut reader)?;
    Ok(SyncSummary {
        peer_name: plan.message,
        sent,
        received,
        tags_received,
        skipped: 0,
    })
}

/// Serve a synchronisation request. Runs on the receiving device.
///
/// `passphrase` unlocks a passphrase-protected archive.
///
/// The reader and writer are handed in rather than created here: the
/// caller has already consumed the opening line from the same stream, and
/// a `BufReader` may have read past that line. Continuing on the very
/// same reader is the only way to guarantee that no byte is lost.
pub fn handle_sync(
    mut reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
    archive_root: &Path,
    request: SyncRequest,
    passphrase: Option<&str>,
) -> Result<SyncReceipt> {
    let mut archive = Archive::open_with_passphrase(archive_root, passphrase)?;
    let local = archive.inventory();
    let tag_filter = request.tag_filter.as_deref();

    // The peer's inventory, narrowed to the requested tag.
    let peer_tagged: Vec<&ItemInfo> = request
        .items
        .iter()
        .filter(|i| matches_tag(i, tag_filter))
        .collect();

    let (send_to_peer, peer_sends) = if request.share_only {
        // One-way push ("share this tag"): the peer sends us its tagged
        // documents and we ask for nothing in return.
        let server_hashes: HashSet<&str> = local.iter().map(|i| i.hash.as_str()).collect();
        let to_send: Vec<TransferItem> = peer_tagged
            .iter()
            .filter(|i| !server_hashes.contains(i.hash.as_str()))
            .map(|i| TransferItem::from(*i))
            .collect();
        (to_send, Vec::new())
    } else {
        // `plan` is written from the server's point of view:
        //   - `server_has` : our files the peer is missing
        //   - `client_has` : the peer's files we are missing
        let (server_has, client_has) =
            plan(&local, &request.items, request.prefer_newest, tag_filter);
        (client_has, server_has)
    };

    let plan_msg = SyncPlan {
        accept: true,
        message: request.sender.clone(),
        send_to_peer: send_to_peer.clone(),
        peer_sends: peer_sends.clone(),
    };
    write_json(&mut writer, &plan_msg)?;

    // 1. Receive the files we are missing.
    let mut received = 0usize;
    let mut tags_received = 0usize;
    for item in &send_to_peer {
        let len = read_length(&mut reader)?;
        let bytes = read_exact_payload(&mut reader, len)?;
        let tags = if request.sync_tags {
            item.tags.clone()
        } else {
            Vec::new()
        };
        if !tags.is_empty() {
            tags_received += 1;
        }
        // With "select newest" the incoming file replaces an older version
        // of the same file instead of piling up next to it.
        if request.prefer_newest {
            archive.import_bytes_newest(
                &bytes,
                &item.filename,
                Some(item.title.clone()),
                tags,
                item.version,
            )?;
        } else {
            archive.import_bytes(
                &bytes,
                &item.filename,
                Some(item.title.clone()),
                tags,
                item.version,
            )?;
        }
        received += 1;
    }

    // 2. Send the files the peer is missing.
    let mut sent = 0usize;
    for item in &peer_sends {
        let bytes = archive.read_blob(&item.hash)?;
        write_payload(&mut writer, &bytes)?;
        sent += 1;
    }
    writer.flush()?;

    let scope = match &request.tag_filter {
        Some(tag) if request.share_only => format!("shared tag '{tag}'"),
        Some(tag) => format!("synced tag '{tag}'"),
        None => "synced everything".to_string(),
    };
    let receipt = SyncReceipt {
        ok: true,
        message: format!(
            "{scope} with {}: {received} file(s) received, {sent} sent",
            request.sender
        ),
        received,
        sent,
        tags_received,
    };
    write_json(&mut writer, &receipt)?;
    Ok(receipt)
}

/// Read an 8-byte little-endian payload length.
fn read_length<R: Read>(reader: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn item(hash: &str, filename: &str, version: Option<DateTime<Utc>>) -> ItemInfo {
        ItemInfo {
            hash: hash.to_string(),
            filename: filename.to_string(),
            size: 10,
            version,
            title: "Doc".to_string(),
            tags: vec!["tag".to_string()],
        }
    }

    fn tagged(hash: &str, filename: &str, tag: &str) -> ItemInfo {
        ItemInfo {
            tags: vec![tag.to_string()],
            ..item(hash, filename, None)
        }
    }

    #[test]
    fn identical_files_are_not_transferred() {
        let a = vec![item("aaa", "a.txt", None)];
        let b = vec![item("aaa", "a.txt", None)];
        let (to_peer, peer_sends) = plan(&a, &b, false, None);
        assert!(to_peer.is_empty());
        assert!(peer_sends.is_empty());
    }

    #[test]
    fn missing_files_are_transferred_both_ways() {
        let a = vec![item("aaa", "a.txt", None)];
        let b = vec![item("bbb", "b.txt", None)];
        let (to_peer, peer_sends) = plan(&a, &b, false, None);
        assert_eq!(to_peer.len(), 1);
        assert_eq!(to_peer[0].hash, "aaa");
        assert_eq!(peer_sends.len(), 1);
        assert_eq!(peer_sends[0].hash, "bbb");
    }

    #[test]
    fn newest_version_wins_and_is_shared() {
        let old = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let new = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        // The peer has the newer version; we do not.
        let local = vec![item("old", "report.pdf", Some(old))];
        let remote = vec![item("new", "report.pdf", Some(new))];
        let (to_peer, peer_sends) = plan(&local, &remote, true, None);
        assert!(to_peer.is_empty(), "the older version must not be pushed");
        assert_eq!(peer_sends.len(), 1);
        assert_eq!(
            peer_sends[0].hash, "new",
            "the newer version must be fetched"
        );

        // We have the newer version; the peer does not.
        let (to_peer, peer_sends) = plan(&remote, &local, true, None);
        assert_eq!(to_peer.len(), 1);
        assert_eq!(to_peer[0].hash, "new");
        assert!(peer_sends.is_empty());
    }

    #[test]
    fn without_prefer_newest_both_versions_are_kept() {
        let old = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let new = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let local = vec![item("old", "report.pdf", Some(old))];
        let remote = vec![item("new", "report.pdf", Some(new))];
        let (to_peer, peer_sends) = plan(&local, &remote, false, None);
        assert_eq!(to_peer.len(), 1);
        assert_eq!(peer_sends.len(), 1);
    }

    #[test]
    fn unknown_versions_are_resolved_deterministically() {
        let local = vec![item("bbb", "report.pdf", None)];
        let remote = vec![item("aaa", "report.pdf", None)];
        let (to_peer, peer_sends) = plan(&local, &remote, true, None);
        // "aaa" is the smaller hash, so it wins and travels exactly one way.
        let total = to_peer.len() + peer_sends.len();
        assert_eq!(total, 1, "exactly one version must be transferred");
        assert_eq!(peer_sends[0].hash, "aaa");

        // The same input must produce the same plan on the other device.
        let (to_peer2, _peer_sends2) = plan(&remote, &local, true, None);
        assert_eq!(to_peer2.len(), 1);
        assert_eq!(to_peer2[0].hash, "aaa");
    }

    #[test]
    fn tag_filter_narrows_the_exchange() {
        let local = vec![
            tagged("aaa", "a.txt", "work"),
            tagged("ccc", "c.txt", "private"),
        ];
        let remote = vec![tagged("bbb", "b.txt", "work")];
        let (to_peer, peer_sends) = plan(&local, &remote, false, Some("work"));
        assert_eq!(to_peer.len(), 1);
        assert_eq!(to_peer[0].hash, "aaa");
        assert_eq!(peer_sends.len(), 1);
        assert_eq!(peer_sends[0].hash, "bbb");
    }
}
