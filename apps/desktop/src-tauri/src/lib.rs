//! Tauri application backend. All archiving logic lives in `panarchiv-core`;
//! this module only bridges UI actions to it.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use panarchiv_core::archive::AddOptions;
use panarchiv_core::bundle;
use panarchiv_core::ocr::{default_engine, recognize_item};
use panarchiv_core::p2p;
use panarchiv_core::{Archive, Document};
use serde::{Deserialize, Serialize};

struct AppState {
    server_stop: Arc<AtomicBool>,
    server_running: Arc<AtomicBool>,
}

fn archive_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".panarchiv")
        .join("archive")
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[derive(Debug, Deserialize)]
pub struct FilePayload {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct PeerDto {
    pub name: String,
    pub addr: String,
}

fn safe_name(name: &str) -> String {
    name.replace(['/', '\\', '\0'], "_")
}

fn materialize(files: &[FilePayload]) -> Result<(PathBuf, Vec<(PathBuf, String)>), String> {
    let archive = Archive::open(archive_root()).map_err(err)?;
    let dir = archive
        .root()
        .join("incoming")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir).map_err(err)?;
    let mut written = Vec::new();
    for f in files {
        let name = safe_name(&f.name);
        let path = dir.join(&name);
        fs::write(&path, &f.bytes).map_err(err)?;
        written.push((path, name));
    }
    Ok((dir, written))
}

#[tauri::command]
fn get_documents() -> Result<Vec<Document>, String> {
    let archive = Archive::open(archive_root()).map_err(err)?;
    Ok(archive.list().to_vec())
}

#[tauri::command]
fn search_documents(query: String) -> Result<Vec<Document>, String> {
    let archive = Archive::open(archive_root()).map_err(err)?;
    Ok(archive.search(&query).into_iter().cloned().collect())
}

#[tauri::command]
fn import_files(
    files: Vec<FilePayload>,
    title: Option<String>,
    tags: Vec<String>,
    group: bool,
    run_ocr: bool,
    language: String,
) -> Result<usize, String> {
    let (dir, written) = materialize(&files)?;
    let paths: Vec<PathBuf> = written.iter().map(|(p, _)| p.clone()).collect();
    let mut archive = Archive::open(archive_root()).map_err(err)?;
    let docs = archive
        .add_files(&paths, &AddOptions { title, tags, group })
        .map_err(err)?;

    if run_ocr {
        let engine = default_engine();
        if engine.available() {
            for doc in &docs {
                for item in &doc.items {
                    if item.kind == panarchiv_core::ItemKind::Image {
                        let _ = recognize_item(
                            &mut archive,
                            &doc.id,
                            &item.id,
                            engine.as_ref(),
                            &language,
                        );
                    }
                }
            }
        }
    }

    let _ = fs::remove_dir_all(&dir);
    Ok(docs.len())
}

#[tauri::command]
fn run_ocr(doc: String, item: String, language: String) -> Result<String, String> {
    let mut archive = Archive::open(archive_root()).map_err(err)?;
    let engine = default_engine();
    let result =
        recognize_item(&mut archive, &doc, &item, engine.as_ref(), &language).map_err(err)?;
    Ok(result.text)
}

#[tauri::command]
fn remove_document(id: String) -> Result<(), String> {
    let mut archive = Archive::open(archive_root()).map_err(err)?;
    archive.remove_document(&id).map_err(err)
}

#[tauri::command]
fn start_server(
    state: tauri::State<'_, AppState>,
    port: u16,
    name: String,
) -> Result<String, String> {
    if state.server_running.swap(true, Ordering::SeqCst) {
        return Err("server is already running".to_string());
    }
    state.server_stop.store(false, Ordering::SeqCst);
    let stop = state.server_stop.clone();
    let root = archive_root();
    std::thread::spawn(move || {
        if let Err(e) = p2p::serve_with_stop(root, port, name, vec![], stop) {
            eprintln!("server error: {e}");
        }
    });
    Ok(format!("Listening on port {port}"))
}

#[tauri::command]
fn stop_server(state: tauri::State<'_, AppState>) -> Result<String, String> {
    if !state.server_running.load(Ordering::SeqCst) {
        return Err("server is not running".to_string());
    }
    state.server_stop.store(true, Ordering::SeqCst);
    state.server_running.store(false, Ordering::SeqCst);
    Ok("Server stopped".to_string())
}

#[tauri::command]
fn discover_peers(port: u16, seconds: u64) -> Result<Vec<PeerDto>, String> {
    let peers = p2p::discover(Duration::from_secs(seconds), port).map_err(err)?;
    Ok(peers
        .into_iter()
        .map(|p| PeerDto {
            name: p.name,
            addr: p.addr.to_string(),
        })
        .collect())
}

#[tauri::command]
fn send_to_peer(
    host: String,
    port: u16,
    name: String,
    files: Vec<FilePayload>,
) -> Result<String, String> {
    let (dir, written) = materialize(&files)?;
    let paths: Vec<PathBuf> = written.iter().map(|(p, _)| p.clone()).collect();
    let addr = p2p::resolve_addr(&host, port).map_err(err)?;
    let receipt = p2p::send_files(addr, &paths, &name).map_err(err)?;
    let _ = fs::remove_dir_all(&dir);
    Ok(format!(
        "Sent {} file(s): {}",
        receipt.received, receipt.message
    ))
}

#[tauri::command]
fn export_bundle(name: String, ids: Vec<String>, all: bool) -> Result<String, String> {
    let archive = Archive::open(archive_root()).map_err(err)?;
    let selection = if all {
        None
    } else if ids.is_empty() {
        return Err("no documents selected".to_string());
    } else {
        Some(ids.as_slice())
    };
    let exports = archive.root().join("exports");
    fs::create_dir_all(&exports).map_err(err)?;
    let dest = exports.join(safe_name(&name));
    let count = bundle::export_bundle(&archive, selection, &dest).map_err(err)?;
    Ok(format!(
        "Exported {count} document(s) to {}",
        dest.display()
    ))
}

#[tauri::command]
fn import_bundle(name: String, bytes: Vec<u8>) -> Result<usize, String> {
    let archive = Archive::open(archive_root()).map_err(err)?;
    let path = archive.root().join("incoming").join(safe_name(&name));
    fs::write(&path, &bytes).map_err(err)?;
    let mut archive = Archive::open(archive_root()).map_err(err)?;
    let ids = bundle::import_bundle(&mut archive, &path).map_err(err)?;
    let _ = fs::remove_file(&path);
    Ok(ids.len())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState {
            server_stop: Arc::new(AtomicBool::new(false)),
            server_running: Arc::new(AtomicBool::new(false)),
        })
        .invoke_handler(tauri::generate_handler![
            get_documents,
            search_documents,
            import_files,
            run_ocr,
            remove_document,
            start_server,
            stop_server,
            discover_peers,
            send_to_peer,
            export_bundle,
            import_bundle
        ])
        .run(tauri::generate_context!())
        .expect("error while running panarchiv");
}
