//! netop — network rules daemon
//!
//! Manages nftables firewall rules and virtual network interfaces on the local
//! node. Runs a reconciliation loop every 5 seconds to enforce desired state.
//!
//! # Backends
//! - **nftables** crate: declarative JSON API to in-kernel nftables
//! - **rtnetlink** crate: veth pairs, bridges, TAP/TUN, routing, namespaces
//! - **ip link** subprocess: VXLAN creation (no Rust crate mature enough)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::time::interval;
use tracing::{error, info, warn};

use proto::{ControlOp, ControlReply, SOCKET_DIR};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct Config {
    /// Reconciliation interval in seconds.
    #[serde(default = "default_reconcile_secs")]
    reconcile_secs: u64,
}
fn default_reconcile_secs() -> u64 { 5 }

// ---------------------------------------------------------------------------
// Desired state types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IfaceKind {
    Veth { peer: String },
    Bridge,
    Tun { tap: bool },
    Vxlan { vni: u32, remote: std::net::Ipv4Addr },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IfaceSpec {
    pub name: String,
    pub kind: IfaceKind,
    pub addresses: Vec<std::net::IpAddr>,
    pub up: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NftRule {
    /// nftables JSON object (passed directly to the nftables crate).
    pub json: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredState {
    pub interfaces: Vec<IfaceSpec>,
    pub nft_rules: Vec<NftRule>,
}

type SharedState = std::sync::Arc<tokio::sync::RwLock<DesiredState>>;

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
async fn reconcile(state: &DesiredState) {
    // --- nftables ---
    for rule in &state.nft_rules {
        // TODO: use nftables crate to apply rule.json
        // let schema: nftables::schema::Nftables = serde_json::from_str(&rule.json).unwrap();
        // nftables::helper::apply_ruleset(&schema, None, None).unwrap();
        tracing::debug!(rule = %rule.json, "nftables rule apply (stub)");
    }

    // --- interfaces via rtnetlink ---
    // TODO: open netlink connection and reconcile:
    //   let (conn, handle, _) = rtnetlink::new_connection()?;
    //   tokio::spawn(conn);
    //   For each IfaceSpec compare desired vs `handle.link().get().execute()`
    for iface in &state.interfaces {
        tracing::debug!(name = %iface.name, "interface reconcile (stub)");
    }
}

#[cfg(not(target_os = "linux"))]
async fn reconcile(_state: &DesiredState) {
    warn!("reconcile: not running on Linux, skipping");
}

async fn reconcile_loop(state: SharedState, period: Duration) {
    let mut tick = interval(period);
    loop {
        tick.tick().await;
        let s = state.read().await;
        reconcile(&s).await;
    }
}

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
async fn control_plane(_state: SharedState) -> Result<()> {
    anyhow::bail!("netop requires Unix/Linux")
}

#[cfg(unix)]
async fn control_plane(state: SharedState) -> Result<()> {
    let socket_path = format!("{SOCKET_DIR}/netop.sock");
    let p = Path::new(&socket_path);
    if p.exists() { std::fs::remove_file(p)?; }
    if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
    let listener = UnixListener::bind(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660))?;
    info!("netop control plane at {socket_path}");

    loop {
        let (mut stream, _) = listener.accept().await?;
        let state = state.clone();
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
                    match tag.as_str() {
                        "state.set" => {
                            match bincode::serde::decode_from_slice::<DesiredState, _>(
                                &inner, bincode::config::standard()
                            ) {
                                Ok((new_state, _)) => {
                                    *state.write().await = new_state;
                                    ControlReply::Ok
                                }
                                Err(e) => ControlReply::Error { message: e.to_string() },
                            }
                        }
                        _ => ControlReply::Error { message: format!("unknown tag: {tag}") },
                    }
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
            .add_directive("netop=info".parse()?))
        .init();

    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/subcluster/netop.toml".into());
    let cfg: Config = if Path::new(&config_path).exists() {
        let raw = std::fs::read_to_string(&config_path)?;
        toml::from_str(&raw)?
    } else { Config::default() };

    let state: SharedState = Default::default();

    tokio::spawn(reconcile_loop(state.clone(), Duration::from_secs(cfg.reconcile_secs)));
    control_plane(state).await
}
