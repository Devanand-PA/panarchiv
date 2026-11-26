# panarchiv

A cross-platform document archiver. Import files, folders and camera captures;
store images, other documents and optional OCR text in one content-addressed
archive; then share documents peer-to-peer over a LAN or as offline bundles.

The repository contains:

| Component | Path | Purpose |
| --- | --- | --- |
| Core library | `crates/panarchiv-core` | Data model, archive storage, OCR abstraction, P2P protocol, bundles |
| CLI | `crates/panarchiv-cli` | `panarchiv` binary for scripting and verification |
| Desktop / mobile app | `apps/desktop` | Tauri 2 GUI (`dist/` holds the frontend, `src-tauri/` the Rust backend) for Windows, macOS, Linux and Android |

## Features

- **Import** individual files or whole folder trees. Files become documents;
  each document holds one or more items (an item is an image, another document
  such as a PDF/DOCX, or any other attachment).
- **Camera capture** on devices that have one. On Android the `Capture photo`
  button uses `accept="image/*" capture="environment"`, which hands off to the
  system camera app. Gallery/file-picker imports work the same way.
- **Optional OCR** via a pluggable `OcrEngine` trait. The desktop backend shells
  out to Tesseract; a native mobile backend can be plugged in without changing
  the archive code. OCR text is searched alongside titles, tags and filenames.
- **Peer-to-peer LAN transfer** with UDP discovery, a TCP file protocol, and
  SHA-256 verification on receipt.
- **Offline bundles** (`.parbundle`) for sneakernet transfer via USB/email/cloud.
- **Content-addressed storage**: every blob is stored under its SHA-256 hash, so
  duplicates are free and integrity is verifiable.

## Archive layout

```text
~/.panarchiv/archive/          # override with --archive or $PANARCHIV_HOME
  manifest.json                # documents, items, OCR text, tags
  blobs/<sha256>               # raw item bytes
  thumbs/<sha256>.png          # generated image thumbnails
  incoming/                    # staging area for P2P / UI imports
  exports/                     # generated offline bundles
```

Set `PANARCHIV_HOME` to move the whole archive elsewhere.

## CLI

```sh
cargo build --workspace

# Create/open an archive
panarchiv init

# Import files (one document per file, or group them as one scan)
panarchiv add scan1.png report.pdf --tag invoices
panarchiv add *.png --group --title "Contract scans"

# Import a folder recursively
panarchiv add-folder ./photos --recursive --tag family

# Inspect / search
panarchiv list
panarchiv show <doc-id>
panarchiv search "invoice"
panarchiv verify

# OCR (requires tesseract on PATH; --engine tesseract|auto)
panarchiv ocr <doc-id> <item-id> --lang eng

# Export a single item
panarchiv export <doc-id> <item-id> ./out.png

# Offline bundles
panarchiv bundle export ./my-docs.parbundle --all
panarchiv bundle import ./my-docs.parbundle

# Peer to peer
panarchiv serve --port 4242 --name "Living room PC"
panarchiv discover --port 4242 --seconds 3
panarchiv send 192.168.1.20 --port 4242 ./file.png ./file.pdf
```

Run `panarchiv --help` or `panarchiv <subcommand> --help` for all options.

## Desktop and Android app

```sh
cd apps/desktop
npm install

# Desktop development
npm run tauri dev

# Desktop installers (.deb, .AppImage, .msi, .dmg, ...)
npm run tauri build

# Android (one-time, requires Android SDK + NDK)
npm run tauri android init
npm run tauri android build        # APK and AAB
```

Android requirements:

- Android Studio SDK + platform tools, and an NDK installed via SDK Manager
- `ANDROID_HOME` (or `ANDROID_SDK_ROOT`) set
- JDK 17 or newer

The app produces both an APK and an AAB for all Android versions supported by
Tauri 2 (Android 7.0 / API 24 and above). Tauri manages the WebView and the
Rust core is shared with desktop, so the archive format is identical everywhere.

### GUI capabilities

- Import files, folders, or a camera photo. Set a title, tags, and optionally
  run OCR immediately after import.
- Browse and search documents; run OCR per image; delete documents.
- Start/stop a LAN sharing endpoint, discover peers, and send selected files
  to a peer by clicking a discovered peer or typing a host/port.
- Export bundles (all documents or a single document) and import bundles.

## OCR on each platform

The core defines `OcrEngine { name, available, recognize }`:

- `TesseractCli` shells out to `tesseract <image> stdout -l <lang>`. It is used
  automatically when `tesseract` is on `PATH`. This covers Windows, macOS and
  Linux (bundle the Tesseract binary with your installer, or require it to be
  installed).
- `NoopOcr` is returned when no engine is present, so OCR is skipped cleanly
  rather than breaking imports.
- **Android:** a device will not have a `tesseract` executable. To enable OCR
  there, implement `OcrEngine` with a native backend (for example Google ML Kit
  Text Recognition through a Tauri plugin) and return it from
  `panarchiv_core::ocr::default_engine()` for mobile targets, or expose a
  dedicated Tauri command in `apps/desktop/src-tauri/src/lib.rs`. The rest of
  the pipeline (storing the text, searching it, bundling it) already works.

## P2P protocol

1. Sender connects to `host:port` and sends a newline-delimited JSON `Hello`
   with its name and a list of file metadata (`name`, `size`, `sha256`).
2. Receiver replies with an accept/reject JSON message.
3. Sender streams each file as a 64-bit little-endian length followed by raw
   bytes.
4. Receiver verifies size and SHA-256 before importing into its archive, then
   sends a JSON receipt.

Discovery uses UDP broadcast on the same port number. Because limited
broadcasts do not always loop back locally, the discoverer also probes
loopback and the local subnet broadcast address.

## Offline bundle format

A `.parbundle` is a gzip-compressed tar archive:

```text
bundle.json          # version, name, timestamp, documents (with OCR text)
blobs/<sha256>       # only the blobs referenced by those documents
```

Import assigns fresh document/item ids to avoid collisions, deduplicates blobs
by hash, and verifies every blob before merging.

## Testing

```sh
cargo test --workspace
```

Current tests cover import/search, grouped imports, and a full bundle
round-trip with integrity verification. The CLI was also exercised end-to-end
against a real Tesseract installation and two local peer processes.

## Known limitations

- The GUI has not been run against a physical Android device from this
  repository; the Android project is generated by `tauri android init` on a
  machine with the Android SDK/NDK. Camera capture relies on the WebView's
  file-input camera handoff rather than a dedicated native camera plugin.
- Large files are buffered in memory during transfer and UI import. For very
  large scans, stream them in chunks.
- P2P is intended for trusted local networks: there is no authentication or
  encryption yet. Use offline bundles over untrusted channels.
