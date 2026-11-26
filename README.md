# panarchiv

A cross-platform document archiver. Import files, folders and camera captures;
store images, other documents and optional OCR text in one **encrypted**,
content-addressed archive; then share documents peer-to-peer over a LAN, sync
two devices, or hand over offline bundles.

The repository contains:

| Component | Path | Purpose |
| --- | --- | --- |
| Core library | `crates/panarchiv-core` | Data model, encrypted archive storage, OCR abstraction, P2P + sync protocols, bundles |
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
- **Encryption at rest.** Every document, thumbnail and the manifest itself are
  encrypted with XChaCha20-Poly1305 before they touch the disk. The archive key
  lives in `key.bin` with `0600` permissions and can optionally be wrapped with
  a passphrase (Argon2id).
- **Dedicated per-app storage.** The archive lives in the platform's own data
  directory — never next to the executable and never relative to the current
  working directory. This is what makes AppImages and Android work reliably.
- **Tags** that behave: a tag edit is never silently lost, even when another
  process writes to the archive at the same time.
- **Peer-to-peer LAN transfer** with UDP discovery, a TCP file protocol, and
  SHA-256 verification on receipt.
- **Device sync.** Two devices compare their inventories and exchange the files
  they are missing, together with the document titles and tags. The optional
  **“select newest”** mode resolves conflicting versions of the same file by
  keeping the most recent one.
- **Share an entire tag** — as a bundle file, or pushed straight to another
  device over the network.
- **Offline bundles** (`.parbundle`) for sneakernet transfer via USB/email/cloud.
- **Content-addressed storage**: every blob is stored under its SHA-256 hash, so
  duplicates are free and integrity is verifiable.

## Where the archive lives

The app never writes next to its own executable. It uses the platform's
dedicated, per-user data directory:

| Platform | Archive location |
| --- | --- |
| Linux | `$XDG_DATA_HOME/com.panarchiv.app/archive` (usually `~/.local/share/com.panarchiv.app/archive`) |
| macOS | `~/Library/Application Support/com.panarchiv.app/archive` |
| Windows | `%APPDATA%\com.panarchiv.app\archive` |
| Android | the app's private storage: `/data/user/0/com.panarchiv.app/archive` |

This matters for two cases in particular:

- **AppImages** run from a read-only squashfs mount and can be launched from an
  arbitrary working directory. Writing next to the `.AppImage` or into the
  current directory either fails or scatters the user's documents across the
  filesystem.
- **Android** gives every app its own private directory. The archive is deleted
  together with the app and is invisible to other apps.

The CLI uses the same idea: `$XDG_DATA_HOME/panarchiv` (or the platform
equivalent), falling back to the legacy location.

An archive created by an older version in `~/.panarchiv/archive` is copied into
the new location automatically the first time the app starts, and is encrypted
on the way.

### Archive layout

```text
<app-data>/archive/
  manifest.json                # documents, items, OCR text, tags  (encrypted)
  key.bin                      # archive encryption key            (0600)
  blobs/<sha256>               # encrypted item bytes
  thumbs/<sha256>.png          # encrypted image thumbnails
  incoming/                    # staging area for P2P / UI imports
  exports/                     # generated offline bundles
  opens/                       # materialised copies for "open with…"
```

Set `PANARCHIV_HOME` (CLI) or `--archive` to move the archive elsewhere.

## Encryption

- The archive key is a random 32-byte key stored in `key.bin`, readable only
  by the current user.
- Every file is encrypted with **XChaCha20-Poly1305** (a 24-byte nonce and an
  authentication tag per file) and written with a self-describing header, so a
  tampered or truncated file is detected instead of silently accepted.
- The SHA-256 that names a blob is computed over the **plaintext**, so two
  devices can compare inventories and deduplicate without decrypting anything.
- You can add a passphrase: the key is then wrapped with a key derived via
  **Argon2id** and the plaintext key never touches the disk. The passphrase is
  kept in memory only — it is never written to disk.
- Archives written by older versions are opened transparently and rewritten
  encrypted the first time.

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
panarchiv tags
panarchiv verify

# Tags
panarchiv tag <doc-id> --tag finance --tag receipts
panarchiv tag <doc-id> --clear

# OCR (requires tesseract on PATH; --engine tesseract|auto)
panarchiv ocr <doc-id> <item-id> --lang eng

# Export a single item
panarchiv export <doc-id> <item-id> ./out.png

# Protect the archive with a passphrase
panarchiv passphrase "correct horse battery staple"
panarchiv --passphrase "correct horse battery staple" list

# Offline bundles
panarchiv bundle export ./my-docs.parbundle --all
panarchiv bundle export ./taxes.parbundle --tag finance
panarchiv bundle import ./my-docs.parbundle

# Peer to peer
panarchiv serve --port 4242 --name "Living room PC"
panarchiv --passphrase "correct horse battery staple" serve --port 4242
panarchiv discover --port 4242 --seconds 3
panarchiv send 192.168.1.20 --port 4242 ./file.png ./file.pdf

# Device sync
panarchiv sync 192.168.1.20 --port 4242 --prefer-newest
panarchiv sync 192.168.1.20 --port 4242 --tag finance
panarchiv share-tag finance 192.168.1.20 --port 4242
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

The app uses a three-pane layout: a **directory tree** on the left, the
selected folder's files in the middle, and a collapsible **tools sidebar** on
the right (import, open-with, peer to peer, device sync, share a tag,
encryption, bundles, and an activity log).

- **Directory view** — documents are filed into folders by their tags. Use a
  `/` in a tag (e.g. `finance/taxes`) to build nested folders. Untagged
  documents live under *Untagged*; *All files* shows everything.
- **Tags** — every card has a **Tags** button that opens an editor: add tags
  one by one or comma-separated, rename them in place, or remove them. Tags
  are stored with the document and are searched alongside titles and OCR text.
- **Open files with any external program** — click a file's **Open** button to
  launch it with your last-used program (or the system default), or use
  **Open with ▾** to pick any registered program, the system default, an
  ad-hoc command, or **Reveal in file manager**. Register programs under *Open
  files with…* using a command template such as `gimp {file}` (if you omit
  `{file}`, the file is passed as the last argument). Because the blob store is
  content-addressed and hash-named, Panarchiv copies each file to a stable,
  extension-bearing path under `<root>/opens/` before handing it off.
  **On Android / iOS**, "open externally" means the *system app chooser*: the
  file is copied into the app cache and launched via an `ACTION_VIEW` intent
  wrapped in `Intent.createChooser`, so the OS shows the familiar "Open with…"
  sheet (image → Photos/editor, PDF → a PDF viewer, …) and remembers the choice.
  The per-program command template is a desktop-only concept and is ignored on
  mobile; "Reveal" also falls back to opening the chooser there.
- Import files, folders, or a camera photo. Set a title, tags, and optionally
  run OCR immediately after import.
- Browse and search documents (titles, tags, filenames, and OCR text); run OCR
  per image; export or delete documents.
- Start/stop a LAN sharing endpoint, discover peers, and send selected files
  to a peer by clicking a discovered peer or typing a host/port.
- **Device sync** — point the app at the other device and press **Sync now**.
  The two devices compare their inventories and exchange exactly the files
  they are missing, along with the titles and tags. Tick **“Select newest
  version when a file differs”** to make the newer version of a conflicting
  file win. The exchange can be narrowed to a single tag.
- **Share a tag** — pick a tag and either export every document carrying it as
  one bundle file, or push it straight to another device on the network.
- **Encryption & storage** — shows where the archive lives, how many documents
  and tags it holds, and lets you add or remove the passphrase.
- Export bundles (all documents, a single document, or a whole tag) and import
  bundles.

### Themes

The header has a theme button that cycles **system → light → dark**. In
*system* mode the app follows the operating system's light/dark preference
live. Both palettes are WCAG AA verified for body text, muted text, accents,
tags and button labels.

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

Two protocols share the same TCP port. The opening message carries a magic
string, so the receiver knows which one is being used.

### `PARCHIV1` — push files

1. Sender connects to `host:port` and sends a newline-delimited JSON `Hello`
   with its name and a list of file metadata (`name`, `size`, `sha256`).
2. Receiver replies with an accept/reject JSON message.
3. Sender streams each file as a 64-bit little-endian length followed by raw
   bytes.
4. Receiver verifies size and SHA-256 before importing into its archive, then
   sends a JSON receipt.

### `PARCHIV2` — device sync

1. Sender connects and sends a JSON `SyncRequest` with its name, the options
   (`prefer_newest`, `sync_tags`, `tag_filter`, `share_only`) and its
   inventory: every blob hash with the filename, size, modification date,
   document title and tags.
2. Receiver computes the same plan and replies with a `SyncPlan` listing the
   files each side has to send.
3. Both sides then stream the missing files using the same length-prefixed
   framing as `PARCHIV1`. The receiver imports each file with the title and
   tags the sender had.
4. A final JSON receipt reports the totals.

**Version resolution.** Identical files are matched by the SHA-256 of their
plaintext, so a file never travels twice. Two devices may hold *different*
versions of the same logical file (matched by filename):

- with **“select newest”** enabled, the newer modification date wins and only
  that version is transferred;
- with it disabled, both versions are kept and both are transferred.

When neither side knows the modification date, the smaller hash wins. The
decision is deterministic, so the two devices always agree and the transfer
cannot ping-pong.

**Sharing a tag.** `share_only` mode turns the exchange into a one-way push:
the sender offers every document carrying the requested tag and asks for
nothing back. The receiver ends up with the whole tag, titles and other tags
intact.

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
by hash, and verifies every blob before merging. The blobs inside a bundle are
plaintext — they are decrypted on export and re-encrypted on import, so a
bundle is a portable copy rather than a piece of the encrypted archive.

Bundles can optionally be encrypted with a passphrase, in which case the file
starts with a `PARBUNDLE2` header and the recipient needs the same passphrase
to open it.

## Concurrency

The same archive may be open in the desktop app, in the CLI and in the P2P
server thread at the same time. Every mutation therefore:

1. takes an exclusive cross-process lock (`flock` / `LockFileEx`),
2. re-reads the manifest from disk,
3. applies the change,
4. writes the manifest to a temporary file and moves it into place.

Because the manifest is re-read under the lock, a writer can never overwrite
somebody else's update with a stale snapshot. This is what makes tags work
reliably.

## Testing

```sh
cargo test --workspace
```

The test suite covers import/search, grouped imports, concurrent tag edits,
encryption round-trips, passphrase wrapping, legacy-archive migration, bundle
round-trips (plain and encrypted), tag-scoped bundles, sync plan computation,
and two end-to-end network tests that run a real server and client over TCP.

## Known limitations

- The GUI has not been run against a physical Android device from this
  repository; the Android project is generated by `tauri android init` on a
  machine with the Android SDK/NDK. Camera capture relies on the WebView's
  file-input camera handoff rather than a dedicated native camera plugin.
- Large files are buffered in memory during transfer and UI import. For very
  large scans, stream them in chunks.
- P2P and device sync are intended for trusted local networks: the transport
  itself has no authentication or encryption, so anyone on the network can
  talk to the sharing endpoint. The data is encrypted *at rest*, but it is sent
  in the clear over the wire. Use an encrypted bundle over untrusted channels.
- A passphrase-protected archive cannot be unlocked by an incoming peer
  connection until the user has unlocked it in the running app.
