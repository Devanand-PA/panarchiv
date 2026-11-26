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

fn write_json<T: Serialize, W: Write>(writer: &mut W, value: &T) -> Result<()> {
    let mut data = serde_json::to_vec(value)?;
    data.push(b'\n');
    writer.write_all(&data)?;
    writer.flush()?;
    Ok(())
}

fn read_json<T: DeserializeOwned, R: BufRead>(reader: &mut R) -> Result<T> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(msg("peer closed the connection"));
    }
    serde_json::from_str(line.trim()).map_err(Into::into)
}

fn write_payload<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
    writer.write_all(bytes)?;
    Ok(())
}

fn read_exact_payload<R: Read>(reader: &mut R, len: u64) -> Result<Vec<u8>> {
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
fn handle_peer(stream: TcpStream, archive_root: &Path, tags: Vec<String>) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let hello: Hello = read_json(&mut reader)?;
    if hello.magic != MAGIC {
        return Err(ArchiveError::Protocol("unknown peer protocol".into()));
    }

    let accept = Response {
        accept: true,
        message: "ready".to_string(),
    };
    let mut writer = stream.try_clone()?;
    write_json(&mut writer, &accept)?;

    let mut archive = Archive::open(archive_root)?;
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
    )
}

/// Like [`serve`], but stops cleanly when `stop` is set. Used by the GUI so a
/// running server can be toggled from a button.
pub fn serve_with_stop(
    archive_root: PathBuf,
    port: u16,
    name: String,
    tags: Vec<String>,
    stop: Arc<AtomicBool>,
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
                std::thread::spawn(move || {
                    if let Err(e) = handle_peer(stream, &root, tags) {
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
            serve_with_stop(root, port, "test-peer".to_string(), vec![], server_stop).unwrap();
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
}
