//! Where the archive lives on disk.
//!
//! Every platform gets a *dedicated* per-user directory for the app rather
//! than a dotfolder in `$HOME`:
//!
//! | Platform | Location |
//! |----------|----------|
//! | Linux    | `$XDG_DATA_HOME/panarchiv` or `~/.local/share/panarchiv` |
//! | macOS    | `~/Library/Application Support/panarchiv` |
//! | Windows  | `%APPDATA%\panarchiv` |
//! | Android  | the app's private storage (`/data/data/<pkg>/files/panarchiv`) |
//!
//! This matters for sandboxed and self-contained builds. An AppImage runs
//! from a read-only squashfs mount and may be launched with a surprising
//! working directory, so writing next to the executable — or relative to the
//! current directory — either fails or silently scatters the archive.
//! `dirs::data_dir` always points at a writable, per-user location, and on
//! Android it is the app's own sandbox, which is deleted with the app.
//!
//! The GUI overrides this with Tauri's `app_data_dir()`, which maps to the
//! same idea but uses the bundle identifier (giving Android its per-app
//! isolation). The CLI and the core use [`default_root`].

use std::path::{Path, PathBuf};

/// The legacy location: `~/.panarchiv`. Kept so existing archives can be
/// migrated instead of being abandoned.
pub fn legacy_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".panarchiv"))
}

/// The default archive root for the CLI and the core crate.
///
/// Returns `None` only if the platform exposes no usable data directory.
pub fn default_root() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("panarchiv"))
}

/// The default archive root, falling back to the legacy location and then
/// to the current directory so callers always get a usable path.
pub fn default_root_or_legacy() -> PathBuf {
    default_root()
        .or_else(legacy_root)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The dedicated root for a Tauri app, given the resolved app data dir.
///
/// `app_data_dir` is expected to already be per-application (on Android it
/// is the app's private files directory).
pub fn app_root(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("archive")
}

/// Move a legacy `~/.panarchiv` archive into the app's dedicated
/// directory.
///
/// The legacy layout was `~/.panarchiv/archive`, so that directory is what
/// gets copied over. The new location is `<app_data>/archive`, which does
/// not line up with the legacy path, so a copy is used rather than a
/// rename (a rename also fails across filesystems).
///
/// Returns the new root when a migration happened, `None` when there was
/// nothing to migrate (no legacy archive, or the new location is already in
/// use).
pub fn migrate_legacy(new_root: &Path) -> Result<Option<PathBuf>, String> {
    let legacy = match legacy_root() {
        Some(p) => p,
        None => return Ok(None),
    };
    if legacy == *new_root {
        return Ok(None);
    }
    let legacy_archive = legacy.join("archive");
    if !legacy_archive.join("manifest.json").exists() {
        return Ok(None);
    }
    // Never overwrite an existing archive.
    if new_root.join("manifest.json").exists() {
        return Ok(None);
    }
    if let Some(parent) = new_root.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    copy_dir(&legacy_archive, new_root).map_err(|e| e.to_string())?;
    Ok(Some(new_root.to_path_buf()))
}

/// Recursively copy a directory.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
