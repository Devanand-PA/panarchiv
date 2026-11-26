//! Tauri application backend. All archiving logic lives in `panarchiv-core`;
//! this module only bridges UI actions to it.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "android"))]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use panarchiv_core::archive::AddOptions;
use panarchiv_core::bundle;
use panarchiv_core::ocr::{default_engine, recognize_item};
use panarchiv_core::p2p;
use panarchiv_core::{Archive, ArchiveItem, Document};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "android")]
use jni::objects::{JClass, JObject, JString, JValue};
#[cfg(target_os = "android")]
use jni::{JNIEnv, JavaVM};
#[cfg(target_os = "android")]
use ndk_context::android_context;

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

/// Build a stable, user-friendly filename for an item to be handed to an
/// external program: the blob store is hash-named and has no extension, so
/// external apps cannot recognise it. We keep the original filename and
/// extension, and append a short item-id fragment so two items that share a
/// filename don't collide. Because the item id is stable, re-opening reuses
/// the same file (external apps can keep it open).
fn open_file_name(item: &ArchiveItem) -> String {
    let safe = safe_name(&item.filename);
    let path = Path::new(&safe);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    let id12: String = item.id.chars().take(8).collect();
    match ext {
        Some(e) if !e.is_empty() => format!("{stem}-{id12}.{e}"),
        _ => format!("{stem}-{id12}"),
    }
}

/// Lowercased extension of a filename (without the dot).
#[cfg(target_os = "android")]
fn file_ext(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Materialise an item's blob into a stable, extension-bearing file under
/// `<root>/opens` and return its path. Desktop-only: Android materialises into
/// the app cache instead (see `android_open_chooser`).
#[cfg(not(target_os = "android"))]
fn materialized_item_path(
    archive: &Archive,
    doc_id: &str,
    item: &ArchiveItem,
) -> Result<PathBuf, String> {
    let opens = archive.root().join("opens");
    fs::create_dir_all(&opens).map_err(err)?;
    let target = opens.join(open_file_name(item));
    if !target.exists() {
        archive.export_item(doc_id, &item.id, &target).map_err(err)?;
    }
    Ok(target)
}

/// Split a command template into arguments, honouring double quotes. Keeps the
/// raw file path intact (the caller substitutes/tokenises the program, not the
/// path) so paths with spaces are never mangled.
#[cfg(not(target_os = "android"))]
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has = true;
            }
            ' ' | '\t' if !in_quotes => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            _ => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Launch a program from a user-supplied template such as `gimp {file}` or
/// `code {file}`. `{file}` / `{path}` are replaced by the file path; if no
/// placeholder is present the file is appended as the final argument.
#[cfg(not(target_os = "android"))]
fn spawn_program(template: &str, file: &Path) -> Result<(), String> {
    let file_str = file.to_string_lossy().into_owned();
    let has_placeholder = template.contains("{file}") || template.contains("{path}");
    let mut args: Vec<String> = tokenize(template)
        .into_iter()
        .map(|tok| tok.replace("{file}", &file_str).replace("{path}", &file_str))
        .collect();
    if !has_placeholder {
        args.push(file_str.clone());
    }
    if args.is_empty() {
        return Err("no program specified".to_string());
    }
    let prog = args.remove(0);
    Command::new(&prog)
        .args(&args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch {prog}: {e}"))
}

/// Open a path with the operating system's default application.
#[cfg(not(target_os = "android"))]
fn open_system_default(path: &Path) -> Result<(), String> {
    let result = (|| -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg(path).spawn().map(|_| ())?;
        }
        #[cfg(target_os = "windows")]
        {
            Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(path)
                .spawn()
                .map(|_| ())?;
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Command::new("xdg-open").arg(path).spawn().map(|_| ())?;
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            let _ = path;
        }
        Ok(())
    })();
    result.map_err(|e| format!("failed to open: {e}"))
}

/// Reveal a file in the OS file manager so the user can open it with whatever
/// program they like from the system context menu.
#[cfg(not(target_os = "android"))]
fn reveal_path(path: &Path) -> Result<(), String> {
    let result = (|| -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg("-R").arg(path).spawn().map(|_| ())?;
        }
        #[cfg(target_os = "windows")]
        {
            Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn()
                .map(|_| ())?;
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let dir = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            Command::new("xdg-open").arg(&dir).spawn().map(|_| ())?;
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            let _ = path;
        }
        Ok(())
    })();
    result.map_err(|e| format!("failed to reveal: {e}"))
}

/// Open a path either with an explicit program or the system default.
#[cfg(not(target_os = "android"))]
fn open_path_with(path: &Path, program: Option<&str>) -> Result<(), String> {
    match program {
        Some(p) if !p.trim().is_empty() => spawn_program(p, path),
        _ => open_system_default(path),
    }
}

/// Minimal dependency-free base64 encoder (used to hand image thumbnails to
/// the WebView as data URLs).
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Best-effort MIME type for an item on Android: ask the platform
/// `MimeTypeMap`, then fall back to the item's recorded MIME, then `*/*`.
#[cfg(target_os = "android")]
fn android_mime(env: &mut JNIEnv<'_>, fallback: &str, ext: &str) -> String {
    if !ext.is_empty() {
        if let Ok(singleton) = env
            .call_static_method(
                "android/webkit/MimeTypeMap",
                "getSingleton",
                "()Landroid/webkit/MimeTypeMap;",
                &[],
            )
            .and_then(|v| v.l())
        {
            if let Ok(ext_jstr) = env.new_string(ext) {
                let ext_obj: JObject = ext_jstr.into();
                if let Ok(m) = env
                    .call_method(
                        &singleton,
                        "getMimeTypeFromExtension",
                        "(Ljava/lang/String;)Ljava/lang/String;",
                        &[JValue::Object(&ext_obj)],
                    )
                    .and_then(|v| v.l())
                {
                    if !m.is_null() {
                        let mjstr: JString = m.into();
                        let s: Option<String> = env.get_string(&mjstr).ok().map(String::from);
                        if let Some(s) = s {
                            if !s.is_empty() {
                                return s;
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = env.exception_clear();
    if fallback.is_empty() {
        "*/*".to_string()
    } else {
        fallback.to_string()
    }
}

/// Materialise an item into the app cache directory and launch the Android
/// "open with another app" chooser for it. This is the mobile equivalent of
/// opening a file with an external program: Android shows the system app
/// selector (Photos, a PDF viewer, GIMP, ...) and remembers the user's choice.
///
/// Android 7 (API 24+) blocks sharing raw `file://` paths across apps, so we
/// hand the target a `content://` URI produced by Tauri's generated
/// `${applicationId}.fileprovider`, granting read permission on the intent.
#[cfg(target_os = "android")]
fn android_open_chooser(archive: &Archive, doc_id: &str, item: &ArchiveItem) -> Result<(), String> {
    let ctx = android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| format!("jvm: {e}"))?;
    let mut env = vm.attach_current_thread().map_err(|e| format!("attach thread: {e}"))?;
    // The activity is a valid `Context`; ndk-context holds a global ref to it.
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    // Resolve the cache directory from the running app (stable across build
    // variants and SDK-driven path changes).
    let cache_file = env
        .call_method(&activity, "getCacheDir", "()Ljava/io/File;", &[])
        .and_then(|v| v.l())
        .map_err(|e| format!("getCacheDir: {e}"))?;
    let cache_abs = env
        .call_method(&cache_file, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(|e| format!("getAbsolutePath: {e}"))?;
    let cache_abs_str: JString = cache_abs.into();
    let cache_dir: String = env
        .get_string(&cache_abs_str)
        .map_err(|e| format!("read cache dir: {e}"))?
        .into();

    let opens_dir = Path::new(&cache_dir).join("panarchiv-open");
    fs::create_dir_all(&opens_dir).map_err(err)?;
    let target = opens_dir.join(open_file_name(item));
    if !target.exists() {
        archive.export_item(doc_id, &item.id, &target).map_err(err)?;
    }

    // Authority is the applicationId (equals getPackageName()) + ".fileprovider",
    // matching Tauri's generated provider in both debug (suffix) and release.
    let pkg_obj = env
        .call_method(&activity, "getPackageName", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(|e| format!("getPackageName: {e}"))?;
    let pkg_jstr: JString = pkg_obj.into();
    let pkg: String = env
        .get_string(&pkg_jstr)
        .map_err(|e| format!("read package: {e}"))?
        .into();
    let authority = format!("{pkg}.fileprovider");

    // java.io.File for the materialised file.
    let target_jstr = env
        .new_string(target.to_string_lossy().into_owned())
        .map_err(|e| format!("new_string: {e}"))?;
    let target_obj: JObject = target_jstr.into();
    let java_file = env
        .new_object("java/io/File", "(Ljava/lang/String;)V", &[JValue::Object(&target_obj)])
        .map_err(|e| format!("new File: {e}"))?;

    // androidx.core.content.FileProvider lives in the APK, so load it through
    // the activity's classloader to work from any thread (not just the UI one).
    let loader = env
        .call_method(&activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])
        .and_then(|v| v.l())
        .map_err(|e| format!("getClassLoader: {e}"))?;
    let fp_name = env
        .new_string("androidx.core.content.FileProvider")
        .map_err(|e| format!("new_string: {e}"))?;
    let fp_name_obj: JObject = fp_name.into();
    let fp_class_obj = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&fp_name_obj)],
        )
        .and_then(|v| v.l())
        .map_err(|e| format!("load FileProvider class: {e}"))?;
    let fp_class = unsafe { JClass::from_raw(fp_class_obj.into_raw()) };

    let authority_jstr = env
        .new_string(authority)
        .map_err(|e| format!("new_string: {e}"))?;
    let authority_obj: JObject = authority_jstr.into();

    // FileProvider.getUriForFile(context, authority, file) -> content:// Uri
    let uri = env
        .call_static_method(
            fp_class,
            "getUriForFile",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/io/File;)Landroid/net/Uri;",
            &[
                JValue::Object(&activity),
                JValue::Object(&authority_obj),
                JValue::Object(&java_file),
            ],
        )
        .and_then(|v| v.l());
    let uri = match uri {
        Ok(u) => u,
        Err(e) => {
            let _ = env.exception_clear();
            return Err(format!(
                "could not get a shareable URI (file not under a FileProvider path?): {e}"
            ));
        }
    };

    let mime = android_mime(&mut env, &item.mime, &file_ext(&item.filename));

    // Intent(ACTION_VIEW) + setDataAndType + grant read permission.
    let action_jstr = env
        .new_string("android.intent.action.VIEW")
        .map_err(|e| format!("new_string: {e}"))?;
    let action_obj: JObject = action_jstr.into();
    let intent = env
        .new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&action_obj)],
        )
        .map_err(|e| format!("new Intent: {e}"))?;
    let mime_jstr = env
        .new_string(mime)
        .map_err(|e| format!("new_string: {e}"))?;
    let mime_obj: JObject = mime_jstr.into();
    env.call_method(
        &intent,
        "setDataAndType",
        "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&uri), JValue::Object(&mime_obj)],
    )
    .map_err(|e| format!("setDataAndType: {e}"))?;
    // Intent.FLAG_GRANT_READ_URI_PERMISSION == 1
    env.call_method(
        &intent,
        "addFlags",
        "(I)Landroid/content/Intent;",
        &[JValue::Int(1)],
    )
    .map_err(|e| format!("addFlags: {e}"))?;

    // Wrap in a chooser so the user picks which app opens the file.
    let title_jstr = env
        .new_string("Open with")
        .map_err(|e| format!("new_string: {e}"))?;
    let title_obj: JObject = title_jstr.into();
    let chooser = env
        .call_static_method(
            "android/content/Intent",
            "createChooser",
            "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;",
            &[JValue::Object(&intent), JValue::Object(&title_obj)],
        )
        .and_then(|v| v.l())
        .map_err(|e| format!("createChooser: {e}"))?;

    if let Err(e) = env.call_method(
        &activity,
        "startActivity",
        "(Landroid/content/Intent;)V",
        &[JValue::Object(&chooser)],
    ) {
        let _ = env.exception_clear();
        return Err(format!("no app available to open this file: {e}"));
    }
    Ok(())
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

/// Materialise an item to an extension-bearing file and open it with an
/// external program (from `program`, e.g. `gimp {file}`) or the OS default
/// when `program` is empty.
///
/// On Android the `program` is ignored and the system app chooser is shown
/// instead (see [`android_open_chooser`]).
#[tauri::command]
#[cfg_attr(target_os = "android", allow(unused_variables))]
fn open_item(doc: String, item: String, program: Option<String>) -> Result<(), String> {
    let archive = Archive::open(archive_root()).map_err(err)?;
    let document = archive
        .get(&doc)
        .ok_or_else(|| "document not found".to_string())?;
    let entry = document
        .items
        .iter()
        .find(|i| i.id == item)
        .ok_or_else(|| "item not found".to_string())?;
    #[cfg(target_os = "android")]
    {
        return android_open_chooser(&archive, &doc, entry);
    }
    #[cfg(not(target_os = "android"))]
    {
        let target = materialized_item_path(&archive, &doc, entry)?;
        open_path_with(&target, program.as_deref())?;
        Ok(())
    }
}

/// Materialise an item and reveal it in the OS file manager. On Android there
/// is no "reveal" concept, so this falls back to opening the chooser.
#[tauri::command]
fn reveal_item(doc: String, item: String) -> Result<(), String> {
    let archive = Archive::open(archive_root()).map_err(err)?;
    let document = archive
        .get(&doc)
        .ok_or_else(|| "document not found".to_string())?;
    let entry = document
        .items
        .iter()
        .find(|i| i.id == item)
        .ok_or_else(|| "item not found".to_string())?;
    #[cfg(target_os = "android")]
    {
        return android_open_chooser(&archive, &doc, entry);
    }
    #[cfg(not(target_os = "android"))]
    {
        let target = materialized_item_path(&archive, &doc, entry)?;
        reveal_path(&target)?;
        Ok(())
    }
}

/// Return an image thumbnail for a blob hash as a base64 data URL, or `None`
/// when no thumbnail exists (non-image or not yet generated).
#[tauri::command]
fn read_thumb(hash: String) -> Option<String> {
    let archive = Archive::open(archive_root()).ok()?;
    let bytes = fs::read(archive.thumb_path(&hash)).ok()?;
    Some(format!("data:image/png;base64,{}", base64(&bytes)))
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
            import_bundle,
            open_item,
            reveal_item,
            read_thumb
        ])
        .run(tauri::generate_context!())
        .expect("error while running panarchiv");
}
