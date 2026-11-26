use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use panarchiv_core::archive::AddOptions;
use panarchiv_core::bundle;
use panarchiv_core::ocr::{default_engine, recognize_item, TesseractCli};
use panarchiv_core::p2p;
use panarchiv_core::paths;
use panarchiv_core::sync::{self, SyncOptions};
use panarchiv_core::{Archive, Document};

#[derive(Debug, Parser)]
#[command(
    name = "panarchiv",
    about = "A cross-platform document archiver",
    version
)]
struct Cli {
    /// Archive directory (defaults to the platform's dedicated data directory)
    #[arg(long, global = true)]
    archive: Option<PathBuf>,

    /// Passphrase protecting the archive key
    #[arg(long, global = true, env = "PANARCHIV_PASSPHRASE")]
    passphrase: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create or open an archive and print its location
    Init,
    /// Import one or more files
    Add {
        paths: Vec<PathBuf>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        tag: Vec<String>,
        /// Combine all files into a single multi-item document
        #[arg(long)]
        group: bool,
    },
    /// Import every file in a directory
    AddFolder {
        dir: PathBuf,
        #[arg(long)]
        recursive: bool,
        #[arg(long)]
        tag: Vec<String>,
    },
    /// List documents
    List,
    /// Show full details for a document
    Show { id: String },
    /// Search titles, tags, filenames and OCR text
    Search { query: String },
    /// List every tag in the archive with its document count
    Tags,
    /// Replace the tags of a document
    Tag {
        /// Document id
        id: String,
        /// Tags to set (repeatable); pass --clear to remove them all
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Remove every tag from the document
        #[arg(long, conflicts_with = "tags")]
        clear: bool,
    },
    /// Run OCR on an image item
    Ocr {
        doc: String,
        item: String,
        #[arg(long, default_value = "eng")]
        lang: String,
        #[arg(long, default_value = "auto", help = "auto, tesseract or none")]
        engine: String,
    },
    /// Copy an item's blob out of the archive
    Export {
        doc: String,
        item: String,
        dest: PathBuf,
    },
    /// Verify every stored blob against its hash
    Verify,
    /// Protect the archive with a passphrase (or remove the protection)
    Passphrase {
        /// The new passphrase; omit to remove the passphrase protection
        value: Option<String>,
    },
    /// Create or read offline archive bundles
    Bundle {
        #[command(subcommand)]
        action: BundleAction,
    },
    /// Listen for incoming peer transfers
    Serve {
        #[arg(long, default_value_t = p2p::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value = "panarchiv")]
        name: String,
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Discover peers on the local network
    Discover {
        #[arg(long, default_value_t = p2p::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Send files to a listening peer
    Send {
        /// Hostname or IP of the peer
        host: String,
        #[arg(long, default_value_t = p2p::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value = "panarchiv")]
        name: String,
        files: Vec<PathBuf>,
    },
    /// Synchronise files and tags with another device
    Sync {
        /// Hostname or IP of the peer
        host: String,
        #[arg(long, default_value_t = p2p::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value = "panarchiv")]
        name: String,
        /// Keep the newer of two versions of the same file
        #[arg(long)]
        prefer_newest: bool,
        /// Only synchronise documents carrying this tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Share every document carrying a tag with another device
    ShareTag {
        /// The tag to share
        tag: String,
        /// Hostname or IP of the peer
        host: String,
        #[arg(long, default_value_t = p2p::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value = "panarchiv")]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum BundleAction {
    /// Export documents to a .parbundle file
    Export {
        dest: PathBuf,
        /// Document ids to export (omit with --all for everything)
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
        /// Export every document carrying this tag
        #[arg(long, conflicts_with_all = ["all", "ids"])]
        tag: Option<String>,
    },
    /// Import documents from a .parbundle file
    Import { path: PathBuf },
}

/// The archive root: `$PANARCHIV_HOME` if set, otherwise the platform's
/// dedicated per-user data directory.
fn default_archive() -> PathBuf {
    if let Ok(home) = std::env::var("PANARCHIV_HOME") {
        return PathBuf::from(home).join("archive");
    }
    paths::default_root_or_legacy()
}

/// Open the archive, supplying the passphrase when one is needed.
fn open_archive(path: &std::path::Path, passphrase: Option<&str>) -> Result<Archive> {
    Archive::open_with_passphrase(path, passphrase).map_err(|e| {
        anyhow!(
            "{e}\n\nThe archive is stored in an encrypted form. Pass --passphrase \
             (or set PANARCHIV_PASSPHRASE) if the archive is passphrase protected."
        )
    })
}

fn print_doc(doc: &Document) {
    println!("[{}] {}", doc.id, doc.title);
    if !doc.tags.is_empty() {
        println!("    tags: {}", doc.tags.join(", "));
    }
    println!("    updated: {}", doc.updated_at.to_rfc3339());
    for item in &doc.items {
        let ocr = item
            .ocr
            .as_ref()
            .map(|o| format!(" | OCR({}): {}", o.language, o.text.replace('\n', " ")))
            .unwrap_or_default();
        let dims = match (item.width, item.height) {
            (Some(w), Some(h)) => format!(" {w}x{h}"),
            _ => String::new(),
        };
        println!(
            "    - {} {} ({:?}, {} bytes){}{}",
            item.id, item.filename, item.kind, item.size, dims, ocr
        );
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let archive_path = cli.archive.clone().unwrap_or_else(default_archive);
    let passphrase = cli.passphrase.as_deref();

    match cli.command {
        Command::Init => {
            let archive = open_archive(&archive_path, passphrase)?;
            println!("Archive ready at {}", archive.root().display());
            println!("Documents are encrypted at rest in this directory.");
        }
        Command::Add {
            paths,
            title,
            tag,
            group,
        } => {
            let mut archive = open_archive(&archive_path, passphrase)?;
            let docs = archive.add_files(
                &paths,
                &AddOptions {
                    title,
                    tags: tag,
                    group,
                },
            )?;
            for doc in &docs {
                println!("Imported {}", doc.id);
                print_doc(doc);
            }
            println!("{} document(s) added", docs.len());
        }
        Command::AddFolder {
            dir,
            recursive,
            tag,
        } => {
            let mut archive = open_archive(&archive_path, passphrase)?;
            let docs = archive.add_folder(&dir, recursive, tag)?;
            println!("Imported {} document(s) from {}", docs.len(), dir.display());
        }
        Command::List => {
            let archive = open_archive(&archive_path, passphrase)?;
            for doc in archive.list() {
                print_doc(doc);
            }
            println!("{} document(s)", archive.list().len());
        }
        Command::Show { id } => {
            let archive = open_archive(&archive_path, passphrase)?;
            let doc = archive
                .get(&id)
                .ok_or_else(|| anyhow!("document not found: {id}"))?;
            print_doc(doc);
        }
        Command::Search { query } => {
            let archive = open_archive(&archive_path, passphrase)?;
            let results = archive.search(&query);
            for doc in &results {
                print_doc(doc);
            }
            println!("{} match(es)", results.len());
        }
        Command::Tags => {
            let archive = open_archive(&archive_path, passphrase)?;
            let tags = archive.tags();
            if tags.is_empty() {
                println!("No tags yet");
            }
            for (tag, count) in tags {
                println!("{tag} ({count} document(s))");
            }
        }
        Command::Tag { id, tags, clear } => {
            let mut archive = open_archive(&archive_path, passphrase)?;
            let new_tags = if clear { Vec::new() } else { tags };
            archive.set_tags(&id, new_tags)?;
            println!("Tags updated for {id}");
        }
        Command::Ocr {
            doc,
            item,
            lang,
            engine,
        } => {
            let mut archive = open_archive(&archive_path, passphrase)?;
            let engine: Box<dyn panarchiv_core::ocr::OcrEngine> = match engine.as_str() {
                "auto" => default_engine(),
                "tesseract" => Box::new(TesseractCli::default()),
                "none" => {
                    return Err(anyhow!("the 'none' engine cannot perform OCR"));
                }
                other => return Err(anyhow!("unknown engine: {other}")),
            };
            let result = recognize_item(&mut archive, &doc, &item, engine.as_ref(), &lang)
                .context("OCR failed")?;
            println!("OCR ({} / {}):", result.engine, result.language);
            println!("{}", result.text);
        }
        Command::Export { doc, item, dest } => {
            let archive = open_archive(&archive_path, passphrase)?;
            archive.export_item(&doc, &item, &dest)?;
            println!("Wrote {}", dest.display());
        }
        Command::Verify => {
            let archive = open_archive(&archive_path, passphrase)?;
            let broken = archive.verify();
            if broken.is_empty() {
                println!("All blobs verified");
            } else {
                println!("{} broken blob(s):", broken.len());
                for hash in broken {
                    println!("  {hash}");
                }
            }
        }
        Command::Passphrase { value } => {
            let mut archive = open_archive(&archive_path, passphrase)?;
            archive.set_passphrase(value.as_deref())?;
            match value {
                Some(_) => println!("The archive is now protected by a passphrase."),
                None => println!("The archive passphrase protection was removed."),
            }
        }
        Command::Bundle { action } => match action {
            BundleAction::Export {
                dest,
                ids,
                all,
                tag,
            } => {
                let archive = open_archive(&archive_path, passphrase)?;
                let count = match tag {
                    Some(tag) => bundle::export_bundle_by_tag(&archive, &tag, &dest, passphrase)?,
                    None => {
                        let selection = if all {
                            None
                        } else if ids.is_empty() {
                            return Err(anyhow!("provide document ids, --tag or --all"));
                        } else {
                            Some(ids.as_slice())
                        };
                        bundle::export_bundle(&archive, selection, &dest, passphrase)?
                    }
                };
                println!("Exported {count} document(s) to {}", dest.display());
            }
            BundleAction::Import { path } => {
                let mut archive = open_archive(&archive_path, passphrase)?;
                let ids = bundle::import_bundle(&mut archive, &path, passphrase)?;
                println!("Imported {} document(s)", ids.len());
                for id in ids {
                    println!("  {id}");
                }
            }
        },
        Command::Serve { port, name, tag } => {
            p2p::serve_with_passphrase(
                archive_path,
                port,
                name,
                tag,
                passphrase.map(str::to_string),
            )?;
        }
        Command::Discover { port, seconds } => {
            let peers = p2p::discover(std::time::Duration::from_secs(seconds), port)?;
            for peer in &peers {
                println!("{} @ {}", peer.name, peer.addr);
            }
            println!("{} peer(s) found", peers.len());
        }
        Command::Send {
            host,
            port,
            name,
            files,
        } => {
            let addr = p2p::resolve_addr(&host, port)?;
            let receipt = p2p::send_files(addr, &files, &name)?;
            if receipt.ok {
                println!("Sent {} file(s): {}", receipt.received, receipt.message);
            } else {
                println!("Peer rejected transfer: {}", receipt.message);
            }
        }
        Command::Sync {
            host,
            port,
            name,
            prefer_newest,
            tag,
        } => {
            let addr = p2p::resolve_addr(&host, port)?;
            let mut archive = open_archive(&archive_path, passphrase)?;
            let options = SyncOptions {
                prefer_newest,
                sync_tags: true,
                tag_filter: tag,
                share_only: false,
            };
            let summary = sync::sync_with_peer(addr, &mut archive, &name, &options)?;
            println!(
                "Synced with {}: {} file(s) received, {} sent, {} with tags",
                summary.peer_name, summary.received, summary.sent, summary.tags_received
            );
        }
        Command::ShareTag {
            tag,
            host,
            port,
            name,
        } => {
            let addr = p2p::resolve_addr(&host, port)?;
            let mut archive = open_archive(&archive_path, passphrase)?;
            let options = SyncOptions::share_tag(tag.clone());
            let summary = sync::sync_with_peer(addr, &mut archive, &name, &options)?;
            println!(
                "Shared tag '{tag}' with {}: {} file(s) sent",
                summary.peer_name, summary.sent
            );
        }
    }

    Ok(())
}
