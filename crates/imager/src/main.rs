//! imager — image creation and P2P distribution daemon
//!
//! Tracks which images are available on which cluster nodes and allows
//! node-to-node image transfer using a chunk-based binary protocol.
//!
//! # Image types
//! - KVM: QCOW2 (created via qemu-img subprocess; read via qcow2 crate)
//! - LXC: rootfs tarball (tar + zstd)
//! - Container: OCI image layout (serde_json structs, no registry)
//!
//! # P2P transport
//! Raw TCP. Each image is split into 64 KiB chunks, each identified by its
//! BLAKE3 hash. A node advertises available chunk hashes; peers request
//! missing chunks directly.
//!
//! # Chunk frame wire format
//! ```text
//! cmd(1B) | chunk_idx(4B BE) | digest(32B) | data_len(4B BE) | data
//! cmd: 0x01=REQUEST 0x02=SEND 0x03=LIST 0x04=NACK
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use proto::{ControlOp, ControlReply, SOCKET_DIR};

const CHUNK_SIZE: usize = 64 * 1024; // 64 KiB

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    /// Root directory for stored images.
    #[serde(default = "default_store_dir")]
    store_dir: String,
    /// TCP port for chunk transfer service.
    #[serde(default = "default_chunk_port")]
    chunk_port: u16,
}
fn default_store_dir() -> String { "/var/lib/hivebus/images".into() }
fn default_chunk_port() -> u16 { 7779 }

impl Default for Config {
    fn default() -> Self {
        Self {
            store_dir: default_store_dir(),
            chunk_port: default_chunk_port(),
        }
    }
}

// ---------------------------------------------------------------------------
// Image catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageKind { Kvm, Lxc, Container }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEntry {
    pub id: String,
    pub kind: ImageKind,
    /// BLAKE3 hex digest of the full image.
    pub blake3: String,
    /// OCI SHA256 hex digest (for container images).
    pub sha256: Option<String>,
    pub size_bytes: u64,
    pub path: String,
}

type Catalog = Arc<RwLock<HashMap<String, ImageEntry>>>;

// ---------------------------------------------------------------------------
// Chunk frame helpers
// ---------------------------------------------------------------------------

#[repr(u8)]
pub enum ChunkCmd { Request = 0x01, Send = 0x02, List = 0x03, Nack = 0x04 }

fn build_request(chunk_idx: u32, digest: &[u8; 32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(37);
    buf.push(ChunkCmd::Request as u8);
    buf.extend_from_slice(&chunk_idx.to_be_bytes());
    buf.extend_from_slice(digest);
    buf
}

// ---------------------------------------------------------------------------
// Image creation helpers
// ---------------------------------------------------------------------------

/// Create a QCOW2 image by calling qemu-img. Returns path to the new image.
fn create_qcow2(store_dir: &str, id: &str, size_gb: u32) -> Result<PathBuf> {
    let path = PathBuf::from(store_dir).join(format!("{id}.qcow2"));
    let status = std::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(&path)
        .arg(format!("{size_gb}G"))
        .status()
        .context("launching qemu-img")?;
    if !status.success() {
        anyhow::bail!("qemu-img failed with status {status}");
    }
    Ok(path)
}

/// Hash a file with BLAKE3, reading in CHUNK_SIZE blocks.
async fn blake3_hash_file(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

async fn handle_image_cmd(tag: &str, _payload: &[u8], catalog: &Catalog, cfg: &Config) -> ControlReply {
    match tag {
        "image.list" => {
            let c = catalog.read().await;
            let entries: Vec<ImageEntry> = c.values().cloned().collect();
            match bincode::serde::encode_to_vec(&entries, bincode::config::standard()) {
                Ok(data) => ControlReply::Data { tag: "image.list".into(), payload: data },
                Err(e) => ControlReply::Error { message: e.to_string() },
            }
        }
        "image.create_kvm" => {
            // TODO: parse spec from _payload
            info!("image.create_kvm (stub — parse WorkloadSpec and call create_qcow2)");
            ControlReply::Ok
        }
        "image.push" => {
            // TODO: parse target node + image id, establish TCP chunk stream
            info!("image.push (stub — open TCP to peer, send chunks)");
            ControlReply::Ok
        }
        _ => ControlReply::Error { message: format!("unknown tag: {tag}") },
    }
}

#[cfg(not(unix))]
async fn control_plane(_catalog: Catalog, _cfg: Arc<Config>) -> Result<()> {
    anyhow::bail!("imager requires Unix/Linux")
}

#[cfg(unix)]
async fn control_plane(catalog: Catalog, cfg: Arc<Config>) -> Result<()> {
    let socket_path = format!("{SOCKET_DIR}/imager.sock");
    let p = Path::new(&socket_path);
    if p.exists() { std::fs::remove_file(p)?; }
    if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
    let listener = UnixListener::bind(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660))?;
    info!("imager control plane at {socket_path}");

    loop {
        let (mut stream, _) = listener.accept().await?;
        let catalog = catalog.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut len_buf = [0u8; 3];
            if stream.read_exact(&mut len_buf).await.is_err() { return; }
            if len_buf[0] != proto::CONTROL_FRAME_MARKER {
                warn!(marker = len_buf[0], "invalid control frame marker");
                return;
            }
            let payload_len = u16::from_be_bytes([len_buf[1], len_buf[2]]) as usize;
            if payload_len > proto::MAX_CONTROL_PAYLOAD {
                warn!(payload_len, "control frame exceeded max payload");
                return;
            }
            let mut payload = vec![0u8; payload_len];
            if stream.read_exact(&mut payload).await.is_err() { return; }

            let reply = match proto::decode_control::<ControlOp>(&payload) {
                Ok(ControlOp::Ping) => ControlReply::Ok,
                Ok(ControlOp::Custom { tag, payload: inner }) =>
                    handle_image_cmd(&tag, &inner, &catalog, &cfg).await,
                Ok(ControlOp::Shutdown) => std::process::exit(0),
                Err(e) => ControlReply::Error { message: e.to_string() },
                _ => ControlReply::Error { message: "unsupported op".into() },
            };
            if let Ok(encoded) = proto::encode_control(&reply) {
                let _ = stream.write_all(&encoded).await;
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("imager=info".parse()?))
        .init();

    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/hivebus/imager.toml".into());
    let cfg = Arc::new(if Path::new(&config_path).exists() {
        let raw = std::fs::read_to_string(&config_path)?;
        toml::from_str::<Config>(&raw)?
    } else {
        Config::default()
    });

    std::fs::create_dir_all(&cfg.store_dir)?;
    let catalog: Catalog = Default::default();

    control_plane(catalog, cfg).await
}
