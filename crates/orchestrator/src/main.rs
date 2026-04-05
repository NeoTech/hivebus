//! orchestrator — virtualization CRUD daemon
//!
//! Manages KVM VMs, LXC containers, and containerd containers on the local node.
//!
//! # Control commands (via Unix socket)
//! All requests are `ControlOp::Custom { tag, payload }` where `tag` is one of:
//!   "vm.create" | "vm.delete" | "vm.start" | "vm.stop" | "vm.list"
//!   "lxc.create" | "lxc.delete" | "lxc.start" | "lxc.stop" | "lxc.list"
//!   "ct.create"  | "ct.delete"  | "ct.start"  | "ct.stop"  | "ct.list"
//!
//! # Dependencies
//! - kvm-ioctls  — /dev/kvm ioctls for KVM lifecycle
//! - nix         — clone(2), pivot_root, namespace flags for LXC
//! - cgroups-rs  — cgroup v2 resource limits
//! - tonic       — gRPC stubs for containerd (TODO: generate from .proto files)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
#[cfg(unix)]
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use proto::{ControlOp, ControlReply, SOCKET_DIR};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    /// Path to containerd socket.
    #[serde(default = "default_containerd_sock")]
    containerd_socket: String,
    /// Default cgroup parent for managed workloads.
    #[serde(default = "default_cgroup_parent")]
    cgroup_parent: String,
    /// Directory for KVM disk images.
    #[serde(default = "default_image_dir")]
    image_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            containerd_socket: default_containerd_sock(),
            cgroup_parent: default_cgroup_parent(),
            image_dir: default_image_dir(),
        }
    }
}

fn default_containerd_sock() -> String { "/run/containerd/containerd.sock".into() }
fn default_cgroup_parent() -> String { "/hivebus".into() }
fn default_image_dir() -> String { "/var/lib/hivebus/images".into() }

impl Config {
    fn load(path: &str) -> Result<Self> {
        if !Path::new(path).exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {path}"))?;
        toml::from_str(&raw).context("parsing config")
    }
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub enum VirtKind { Kvm, Lxc, Container }

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkloadSpec {
    pub id: String,
    pub kind: VirtKind,
    pub image: String,
    pub cpus: u32,
    pub memory_mb: u64,
    pub extra: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkloadStatus {
    pub id: String,
    pub kind: VirtKind,
    pub running: bool,
    pub pid: Option<i32>,
}

// ---------------------------------------------------------------------------
// KVM backend (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod kvm_backend {
    use anyhow::Result;
    use kvm_ioctls::Kvm;
    use tracing::info;

    pub fn is_available() -> bool {
        Kvm::new().is_ok()
    }

    pub fn create_vm(id: &str, _memory_mb: u64) -> Result<()> {
        let kvm = Kvm::new()?;
        let _vm = kvm.create_vm()?;
        info!(id, "KVM VM slot created (stub — vCPU/memory setup TODO)");
        // TODO: create vCPUs, map memory regions, load firmware/kernel image,
        //       set up virtio devices, persist VM descriptor.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// LXC backend (Linux namespaces, nix crate)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod lxc_backend {
    use anyhow::Result;
    use tracing::info;

    pub fn create_container(id: &str, rootfs: &str) -> Result<()> {
        info!(id, rootfs, "LXC container create (stub)");
        // TODO:
        // 1. Unpack rootfs tarball via `tar` crate into /var/lib/hivebus/lxc/<id>/
        // 2. Fork child with nix::sched::clone():
        //      CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC
        // 3. pivot_root(2) into container rootfs
        // 4. Drop capabilities with nix::sys::prctl / caps crate
        // 5. Set up cgroup v2 slice via cgroups-rs
        // 6. exec init process
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// containerd backend (gRPC via tonic)
// ---------------------------------------------------------------------------

mod containerd_backend {
    use anyhow::Result;
    use tracing::info;

    /// TODO: Generate tonic stubs from containerd .proto files:
    ///   https://github.com/containerd/containerd/tree/main/api
    /// Add `tonic` and `prost` to Cargo.toml, run tonic-build in build.rs.
    pub async fn create_container(id: &str, image: &str, _sock: &str) -> Result<()> {
        info!(id, image, "containerd container create (stub — tonic TODO)");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

async fn handle_command(tag: &str, payload: &[u8], cfg: &Config) -> ControlReply {
    info!(tag, "orchestrator command");
    match tag {
        "vm.create" => {
            let Ok((spec, _)) = bincode::serde::decode_from_slice::<WorkloadSpec, _>(
                payload, bincode::config::standard()
            ) else {
                return ControlReply::Error { message: "invalid payload".into() };
            };
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = kvm_backend::create_vm(&spec.id, spec.memory_mb) {
                    return ControlReply::Error { message: e.to_string() };
                }
            }
            ControlReply::Ok
        }
        "lxc.create" => {
            let Ok((spec, _)) = bincode::serde::decode_from_slice::<WorkloadSpec, _>(
                payload, bincode::config::standard()
            ) else {
                return ControlReply::Error { message: "invalid payload".into() };
            };
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = lxc_backend::create_container(&spec.id, &spec.image) {
                    return ControlReply::Error { message: e.to_string() };
                }
            }
            ControlReply::Ok
        }
        _ => ControlReply::Error { message: format!("unknown tag: {tag}") },
    }
}

#[cfg(not(unix))]
async fn control_plane(_cfg: std::sync::Arc<Config>) -> Result<()> {
    anyhow::bail!("orchestrator requires Unix/Linux")
}

#[cfg(unix)]
async fn control_plane(cfg: std::sync::Arc<Config>) -> Result<()> {
    let socket_path = format!("{SOCKET_DIR}/orchestrator.sock");
    let p = Path::new(&socket_path);
    if p.exists() { std::fs::remove_file(p)?; }
    if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
    let listener = UnixListener::bind(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660))?;
    info!("orchestrator control plane at {socket_path}");

    loop {
        let (mut stream, _) = listener.accept().await?;
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
                Ok(ControlOp::Custom { tag, payload: inner }) => {
                    handle_command(&tag, &inner, &cfg).await
                }
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
            .add_directive("orchestrator=info".parse()?))
        .init();

    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/hivebus/orchestrator.toml".into());
    let cfg = std::sync::Arc::new(Config::load(&config_path)?);

    #[cfg(target_os = "linux")]
    info!(kvm_available = kvm_backend::is_available(), "orchestrator starting");

    control_plane(cfg).await
}
