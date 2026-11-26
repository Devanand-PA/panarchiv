//! Peer-to-peer transfer over the local network plus lightweight discovery.
//!
//! Wire format: newline-delimited JSON control messages, followed by
//! length-prefixed binary payloads. Every payload carries its SHA-256 in the
//! header so the receiver can verify integrity before importing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::archive::{sha256_hex, unique_path, Archive};
use crate::error::{msg, ArchiveError, Result};

pub const DEFAULT_PORT: u16 = 4242;
const MAGIC: &str = "PARCHIV1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    magic: String,
    sender: String,
    files: Vec<FileMeta>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    accept: bool,
    message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub ok: bool,
    pub message: String,
    pub received: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiscoveryPacket {
    kind: String,
    name: String,
    port: u16,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub name: String,
    pub addr: SocketAddr,
}

pub(crate) fn write_json<T: Serialize, W: Write>(writer: &mut W, value: &T) -> Result<()> {
    let mut data = serde_json::to_vec(value)?;
    data.push(b'\n');
    writer.write_all(&data)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn read_json<T: DeserializeOwned, R: BufRead>(reader: &mut R) -> Result<T> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(msg("peer closed the connection"));
    }
    serde_json::from_str(line.trim()).map_err(Into::into)
}

pub(crate) fn write_payload<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
    writer.write_all(bytes)?;
    Ok(())
}

pub(crate) fn read_exact_payload<R: Read>(reader: &mut R, len: u64) -> Result<Vec<u8>> {
    if len > 8 * 1024 * 1024 * 1024 {
        return Err(msg("refusing payload larger than 8 GiB"));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

fn file_meta(path: &Path) -> Result<FileMeta> {
    let bytes = std::fs::read(path)?;
    Ok(FileMeta {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".to_string()),
        size: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
    })
}

/// Send files to a listening peer. Returns true when the peer accepted all
/// files and verified their checksums.
pub fn send_files(addr: SocketAddr, paths: &[PathBuf], sender_name: &str) -> Result<Receipt> {
    if paths.is_empty() {
        return Err(msg("no files to send"));
    }
    let mut metas = Vec::new();
    for path in paths {
        if !path.is_file() {
            return Err(msg(format!("not a file: {}", path.display())));
        }
        metas.push(file_meta(path)?);
    }

    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let hello = Hello {
        magic: MAGIC.to_string(),
        sender: sender_name.to_string(),
        files: metas.clone(),
    };
    write_json(&mut writer, &hello)?;

    let response: Response = read_json(&mut reader)?;
    if !response.accept {
        return Ok(Receipt {
            ok: false,
            message: response.message,
            received: 0,
        });
    }

    for (path, meta) in paths.iter().zip(metas.iter()) {
        let bytes = std::fs::read(path)?;
        if sha256_hex(&bytes) != meta.sha256 {
            return Err(msg(format!(
                "local file changed while sending: {}",
                meta.name
            )));
        }
        write_payload(&mut writer, &bytes)?;
    }
    writer.flush()?;

    let receipt: Receipt = read_json(&mut reader)?;
    Ok(receipt)
}

/// Handle a single inbound connection.
///
/// Two protocols share the port: `PARCHIV1` pushes a handful of files to
/// the peer, `PARCHIV2` reconciles the two archives (see [`crate::sync`]).
/// The opening message carries the magic, so the receiver can tell them
/// apart before committing to either shape.
fn handle_peer(
    stream: TcpStream,
    archive_root: &Path,
    tags: Vec<String>,
    passphrase: Option<&str>,
) -> Result<()> {
    let mut reader = BufReader::new(&stream);

    // Peek at the magic first: the two protocols use different opening
    // messages, so the whole line has to be read before it can be parsed
    // as either shape.
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(msg("peer closed the connection"));
    }
    let magic = serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|v| v.get("magic").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_default();

    if magic == crate::sync::SYNC_MAGIC {
        let request: crate::sync::SyncRequest = serde_json::from_str(line.trim())?;
        let receipt = crate::sync::handle_sync(stream, archive_root, request, passphrase)?;
        println!("{}", receipt.message);
        return Ok(());
    }

    let hello: Hello = serde_json::from_str(line.trim())?;
    if hello.magic != MAGIC {
        return Err(ArchiveError::Protocol("unknown peer protocol".into()));
    }

    let accept = Response {
        accept: true,
        message: "ready".to_string(),
    };
    let mut writer = stream.try_clone()?;
    write_json(&mut writer, &accept)?;

    let mut archive = Archive::open_with_passphrase(archive_root, passphrase)?;
    let mut received = 0usize;
    for meta in &hello.files {
        let len_buf = {
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            u64::from_le_bytes(buf)
        };
        let bytes = read_exact_payload(&mut reader, len_buf)?;
        if bytes.len() as u64 != meta.size {
            return Err(ArchiveError::ChecksumMismatch {
                name: meta.name.clone(),
                expected: meta.sha256.clone(),
                actual: format!("size {} != {}", bytes.len(), meta.size),
            });
        }
        let actual = sha256_hex(&bytes);
        if actual != meta.sha256 {
            return Err(ArchiveError::ChecksumMismatch {
                name: meta.name.clone(),
                expected: meta.sha256.clone(),
                actual,
            });
        }
        let incoming_dir = archive.root().join("incoming");
        let temp = unique_path(&incoming_dir.join(&meta.name));
        std::fs::write(&temp, &bytes)?;
        archive.import_file(&temp, &meta.name, None, tags.clone())?;
        let _ = std::fs::remove_file(&temp);
        received += 1;
    }

    let receipt = Receipt {
        ok: true,
        message: format!("received {} file(s) from {}", received, hello.sender),
        received,
    };
    write_json(&mut writer, &receipt)?;
    Ok(())
}

/// Serve files indefinitely. Also answers UDP discovery requests on the same
/// port number.
pub fn serve(archive_root: PathBuf, port: u16, name: String, tags: Vec<String>) -> Result<()> {
    serve_with_stop(
        archive_root,
        port,
        name,
        tags,
        Arc::new(AtomicBool::new(false)),
        None,
    )
}

/// Like [`serve`], but unlocks a passphrase-protected archive so the
/// endpoint can accept incoming transfers and sync requests.
pub fn serve_with_passphrase(
    archive_root: PathBuf,
    port: u16,
    name: String,
    tags: Vec<String>,
    passphrase: Option<String>,
) -> Result<()> {
    serve_with_stop(
        archive_root,
        port,
        name,
        tags,
        Arc::new(AtomicBool::new(false)),
        passphrase,
    )
}

/// Like [`serve`], but stops cleanly when `stop` is set. Used by the GUI so a
/// running server can be toggled from a button.
///
/// `passphrase` unlocks a passphrase-protected archive so the server can
/// accept incoming transfers and sync requests.
pub fn serve_with_stop(
    archive_root: PathBuf,
    port: u16,
    name: String,
    tags: Vec<String>,
    stop: Arc<AtomicBool>,
    passphrase: Option<String>,
) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
    listener.set_nonblocking(true)?;
    spawn_discovery_responder(port, name.clone(), stop.clone());

    println!("Listening on 0.0.0.0:{port} as '{name}'");
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let root = archive_root.clone();
                let tags = tags.clone();
                let passphrase = passphrase.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_peer(stream, &root, tags, passphrase.as_deref()) {
                        eprintln!("peer transfer failed: {e}");
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
    println!("Server stopped");
    Ok(())
}

fn spawn_discovery_responder(port: u16, name: String, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("discovery bind failed: {e}");
                return;
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("set timeout");
        let mut buf = [0u8; 2048];
        while !stop.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buf) {
                Ok((len, peer)) => {
                    if let Ok(packet) = serde_json::from_slice::<DiscoveryPacket>(&buf[..len]) {
                        if packet.kind == "HELLO" {
                            let reply = DiscoveryPacket {
                                kind: "PEER".to_string(),
                                name: name.clone(),
                                port,
                            };
                            if let Ok(data) = serde_json::to_vec(&reply) {
                                let _ = socket.send_to(&data, peer);
                            }
                        }
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    eprintln!("discovery error: {e}");
                    break;
                }
            }
        }
    });
}

/// Broadcast a discovery request and collect responding peers.
pub fn discover(timeout: Duration, port: u16) -> Result<Vec<Peer>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    let hello = DiscoveryPacket {
        kind: "HELLO".to_string(),
        name: "discover".to_string(),
        port: 0,
    };
    let data = serde_json::to_vec(&hello)?;
    socket.send_to(&data, (Ipv4Addr::BROADCAST, port))?;
    // Some networks (and containers) do not loop limited broadcasts back to
    // local sockets, so also probe loopback and the local /24 broadcast.
    let _ = socket.send_to(&data, (Ipv4Addr::LOCALHOST, port));
    if let IpAddr::V4(ip) = local_ip() {
        let o = ip.octets();
        let _ = socket.send_to(&data, (Ipv4Addr::new(o[0], o[1], o[2], 255), port));
    }

    let deadline = Instant::now() + timeout;
    let mut peers: HashMap<SocketAddr, Peer> = HashMap::new();
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((len, addr)) => {
                if let Ok(packet) = serde_json::from_slice::<DiscoveryPacket>(&buf[..len]) {
                    if packet.kind == "PEER" {
                        let peer_addr = SocketAddr::new(addr.ip(), packet.port);
                        peers.entry(peer_addr).or_insert(Peer {
                            name: packet.name,
                            addr: peer_addr,
                        });
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut out: Vec<Peer> = peers.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn resolve_addr(host: &str, port: u16) -> Result<SocketAddr> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| msg(format!("cannot resolve {host}:{port}")))?;
    Ok(addr)
}

pub fn local_ip() -> IpAddr {
    // A cheap, dependency-free way to learn the LAN address: connect a UDP
    // socket to a public address (no packets are actually sent).
    match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        Ok(s) => match s.connect("8.8.8.8:80") {
            Ok(_) => s
                .local_addr()
                .map(|a| a.ip())
                .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Err(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        },
        Err(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_imports_verified_file() {
        let dir = std::env::temp_dir().join(format!("pa-p2p-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, "peer transfer payload").unwrap();

        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let root = dir.join("dst");
        let server = std::thread::spawn(move || {
            serve_with_stop(root, port, "test-peer".to_string(), vec![], server_stop, None).unwrap();
        });
        std::thread::sleep(Duration::from_millis(150));

        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let receipt = send_files(addr, &[file], "sender").unwrap();
        assert!(receipt.ok);
        assert_eq!(receipt.received, 1);

        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();

        let archive = Archive::open(dir.join("dst")).unwrap();
        assert_eq!(archive.list().len(), 1);
        assert_eq!(archive.search("hello.txt").len(), 1);
        assert!(archive.verify().is_empty());
    }

    /// Two devices exchange their archives over the wire. Each side holds
    /// one file the other does not, so both directions have to be exercised.
    #[test]
    fn sync_exchanges_missing_files_in_both_directions() {
        let dir = std::env::temp_dir().join(format!("pa-sync-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut local = Archive::open(dir.join("local")).unwrap();
        let local_file = dir.join("local.txt");
        std::fs::write(&local_file, "from the local device").unwrap();
        local
            .add_files(
                &[local_file],
                &crate::archive::AddOptions {
                    tags: vec!["shared".into()],
                    ..Default::default()
                },
            )
            .unwrap();

        let remote_root = dir.join("remote");
        let mut remote = Archive::open(&remote_root).unwrap();
        let remote_file = dir.join("remote.txt");
        std::fs::write(&remote_file, "from the remote device").unwrap();
        remote
            .add_files(
                &[remote_file],
                &crate::archive::AddOptions {
                    tags: vec!["shared".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        drop(remote);

        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server_root = remote_root.clone();
        let server = std::thread::spawn(move || {
            serve_with_stop(
                server_root,
                port,
                "remote-device".to_string(),
                vec![],
                server_stop,
                None,
            )
            .unwrap();
        });
        std::thread::sleep(Duration::from_millis(150));

        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let summary = crate::sync::sync_with_peer(
            addr,
            &mut local,
            "local-device",
            &crate::sync::SyncOptions::full_sync(),
        )
        .unwrap();
        assert_eq!(summary.sent, 1, "the local file must be sent");
        assert_eq!(summary.received, 1, "the remote file must be received");
        assert_eq!(summary.tags_received, 1, "the tag must travel with the file");

        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();

        // Both sides now hold both files.
        assert_eq!(local.list().len(), 2);
        let remote = Archive::open(&remote_root).unwrap();
        assert_eq!(remote.list().len(), 2);
        assert_eq!(remote.search("remote.txt").len(), 1);
        assert_eq!(remote.search("local.txt").len(), 1);
        assert!(remote.verify().is_empty());
    }

    /// Sharing a tag pushes the tagged documents and asks for nothing back.
    #[test]
    fn sharing_a_tag_pushes_only_that_tag() {        let dir = std::env::temp_dir().join(format!("pa-share-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut local = Archive::open(dir.join("local")).unwrap();
        for (name, tag) in [("public.txt", "public"), ("secret.txt", "private")] {
            let path = dir.join(name);
            std::fs::write(&path, name).unwrap();
            local
                .add_files(
                    &[path],
                    &crate::archive::AddOptions {
                        tags: vec![tag.into()],
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let remote_root = dir.join("remote");
        Archive::open(&remote_root).unwrap();

        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server_root = remote_root.clone();
        let server = std::thread::spawn(move || {
            serve_with_stop(
                server_root,
                port,
                "remote-device".to_string(),
                vec![],
                server_stop,
                None,
            )
            .unwrap();
        });
        std::thread::sleep(Duration::from_millis(150));

        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let summary = crate::sync::sync_with_peer(
            addr,
            &mut local,
            "local-device",
            &crate::sync::SyncOptions::share_tag("public"),
        )
        .unwrap();
        assert_eq!(summary.sent, 1, "only the tagged file is sent");
        assert_eq!(summary.received, 0, "sharing asks for nothing back");

        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();

        let remote = Archive::open(&remote_root).unwrap();
        assert_eq!(remote.list().len(), 1);
        assert_eq!(remote.list()[0].tags, vec!["public".to_string()]);
        assert!(remote.verify().is_empty());
    }

    /// With "select newest" the device that was missing the newer version
    /// receives it and drops the stale copy, instead of ending up with two
    /// documents of the same name.
    #[test]
    fn prefer_newest_replaces_the_stale_version() {
        let dir = std::env::temp_dir().join(format!("pa-newest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let old = chrono::Utc::now() - chrono::Duration::days(30);
        let new = chrono::Utc::now();

        // The local device holds the old version...
        let mut local = Archive::open(dir.join("local")).unwrap();
        local
            .import_bytes(
                b"old version",
                "report.pdf",
                Some("Report".into()),
                vec!["work".into()],
                Some(old),
            )
            .unwrap();
        assert_eq!(local.list().len(), 1);

        // ...and the peer holds the new one.
        let remote_root = dir.join("remote");
        let mut remote = Archive::open(&remote_root).unwrap();
        remote
            .import_bytes(
                b"new version",
                "report.pdf",
                Some("Report".into()),
                vec!["work".into()],
                Some(new),
            )
            .unwrap();
        drop(remote);

        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server_root = remote_root.clone();
        let server = std::thread::spawn(move || {
            serve_with_stop(
                server_root,
                port,
                "remote-device".to_string(),
                vec![],
                server_stop,
                None,
            )
            .unwrap();
        });
        std::thread::sleep(Duration::from_millis(150));

        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let summary = crate::sync::sync_with_peer(
            addr,
            &mut local,
            "local-device",
            &crate::sync::SyncOptions {
                prefer_newest: true,
                sync_tags: true,
                tag_filter: None,
                share_only: false,
            },
        )
        .unwrap();
        assert_eq!(summary.received, 1, "the newer version is fetched");
        assert_eq!(summary.sent, 0, "the older version is not pushed");

        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();

        // The stale copy is gone: exactly one document remains and it
        // holds the newer contents.
        assert_eq!(local.list().len(), 1, "the stale version must be replaced");
        let doc = &local.list()[0];
        assert_eq!(doc.items.len(), 1);
        let bytes = local.read_blob(&doc.items[0].blob).unwrap();
        assert_eq!(bytes, b"new version");
        assert!(local.verify().is_empty());
    }
}
