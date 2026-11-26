use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use panarchiv_core::archive::AddOptions;
use panarchiv_core::bundle;
use panarchiv_core::ocr::{default_engine, recognize_item, TesseractCli};
use panarchiv_core::p2p;
use panarchiv_core::{Archive, Document};

#[derive(Debug, Parser)]
#[command(name = "panarchiv", about = "A cross-platform document archiver")]
struct Cli {
    /// Archive directory (defaults to $PANARCHIV_HOME or ~/.panarchiv)
    #[arg(long, global = true)]
    archive: Option<PathBuf>,

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
    },
    /// Import documents from a .parbundle file
    Import { path: PathBuf },
}

fn default_archive() -> PathBuf {
    if let Ok(home) = std::env::var("PANARCHIV_HOME") {
        return PathBuf::from(home).join("archive");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".panarchiv")
        .join("archive")
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
    let archive_path = cli.archive.unwrap_or_else(default_archive);

    match cli.command {
        Command::Init => {
            let archive = Archive::open(&archive_path)?;
            println!("Archive ready at {}", archive.root().display());
        }
        Command::Add {
            paths,
            title,
            tag,
            group,
        } => {
            let mut archive = Archive::open(&archive_path)?;
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
            let mut archive = Archive::open(&archive_path)?;
            let docs = archive.add_folder(&dir, recursive, tag)?;
            println!("Imported {} document(s) from {}", docs.len(), dir.display());
        }
        Command::List => {
            let archive = Archive::open(&archive_path)?;
            for doc in archive.list() {
                print_doc(doc);
            }
            println!("{} document(s)", archive.list().len());
        }
        Command::Show { id } => {
            let archive = Archive::open(&archive_path)?;
            let doc = archive
                .get(&id)
                .ok_or_else(|| anyhow!("document not found: {id}"))?;
            print_doc(doc);
        }
        Command::Search { query } => {
            let archive = Archive::open(&archive_path)?;
            let results = archive.search(&query);
            for doc in &results {
                print_doc(doc);
            }
            println!("{} match(es)", results.len());
        }
        Command::Ocr {
            doc,
            item,
            lang,
            engine,
        } => {
            let mut archive = Archive::open(&archive_path)?;
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
            let archive = Archive::open(&archive_path)?;
            archive.export_item(&doc, &item, &dest)?;
            println!("Wrote {}", dest.display());
        }
        Command::Verify => {
            let archive = Archive::open(&archive_path)?;
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
        Command::Bundle { action } => match action {
            BundleAction::Export { dest, ids, all } => {
                let archive = Archive::open(&archive_path)?;
                let selection = if all {
                    None
                } else if ids.is_empty() {
                    return Err(anyhow!("provide document ids or pass --all"));
                } else {
                    Some(ids.as_slice())
                };
                let count = bundle::export_bundle(&archive, selection, &dest)?;
                println!("Exported {count} document(s) to {}", dest.display());
            }
            BundleAction::Import { path } => {
                let mut archive = Archive::open(&archive_path)?;
                let ids = bundle::import_bundle(&mut archive, &path)?;
                println!("Imported {} document(s)", ids.len());
                for id in ids {
                    println!("  {id}");
                }
            }
        },
        Command::Serve { port, name, tag } => {
            p2p::serve(archive_path, port, name, tag)?;
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
    }

    Ok(())
}
