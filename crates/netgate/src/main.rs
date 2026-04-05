//! netgate — network gateway daemon
//!
//! Provides ingress/egress control, internal DNS, and meta-tagged routing
//! rules for the cluster network.
//!
//! # Components
//! - **hickory-server**: authoritative DNS for `cluster.internal` zone
//! - **tokio TCP proxy**: L4 ingress/egress forwarding rules
//! - **aya (eBPF/XDP)**: fast-path packet filtering (TODO: requires aya-build)
//! - **tun** crate: TUN interface for gateway traffic
//!
//! # DNS control
//! Records are managed programmatically via `InMemoryZoneHandler::upsert()`.
//! No web API — the Unix socket control plane issues upsert/delete commands.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use proto::{ControlOp, ControlReply, SOCKET_DIR};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    /// DNS listen address.
    #[serde(default = "default_dns_addr")]
    dns_addr: SocketAddr,
    /// Internal cluster domain suffix.
    #[serde(default = "default_domain")]
    domain: String,
    /// Optional TUN interface name for gateway.
    tun_iface: Option<String>,
}
fn default_dns_addr() -> SocketAddr { "0.0.0.0:5353".parse().unwrap() }
fn default_domain() -> String { "cluster.internal".into() }

// ---------------------------------------------------------------------------
// DNS record table (in-memory; backed by hickory-server in full impl)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecord {
    pub name: String,
    pub ttl: u32,
    pub addr: IpAddr,
}

type DnsTable = Arc<RwLock<HashMap<String, DnsRecord>>>;

// ---------------------------------------------------------------------------
// Proxy rule table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRule {
    pub id: String,
    pub listen: SocketAddr,
    pub target: SocketAddr,
}

type ProxyRules = Arc<RwLock<Vec<ProxyRule>>>;

// ---------------------------------------------------------------------------
// DNS server (hickory-server stub)
// ---------------------------------------------------------------------------

async fn dns_server(table: DnsTable, addr: SocketAddr) {
    // TODO: Full implementation:
    //   use hickory_server::ServerFuture;
    //   use hickory_server::store::in_memory::InMemoryAuthority;
    //   1. Create InMemoryAuthority for the cluster.internal zone
    //   2. On each DNS query, look up `table` for the queried name
    //   3. Return A/AAAA record from table, NXDOMAIN if not found
    //   4. Support DNS UPDATE (RFC 2136) to add/remove records programmatically
    info!(%addr, "DNS server listening (stub — hickory-server integration TODO)");
    // Placeholder: simply park the task.
    std::future::pending::<()>().await;
}

// ---------------------------------------------------------------------------
// L4 proxy
// ---------------------------------------------------------------------------

async fn run_proxy_rule(rule: ProxyRule) {
    use tokio::io::copy_bidirectional;
    use tokio::net::{TcpListener, TcpStream};

    let listener = match TcpListener::bind(rule.listen).await {
        Ok(l) => l,
        Err(e) => { error!(listen = %rule.listen, "proxy bind failed: {e}"); return; }
    };
    info!(id = %rule.id, listen = %rule.listen, target = %rule.target, "proxy rule active");
    loop {
        match listener.accept().await {
            Ok((mut inbound, from)) => {
                let target = rule.target;
                tokio::spawn(async move {
                    match TcpStream::connect(target).await {
                        Ok(mut outbound) => {
                            if let Err(e) = copy_bidirectional(&mut inbound, &mut outbound).await {
                                // Connection closed normally or with error — not worth logging.
                                let _ = e;
                            }
                        }
                        Err(e) => warn!(%from, %target, "proxy connect failed: {e}"),
                    }
                });
            }
            Err(e) => error!("proxy accept error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
async fn control_plane(_dns_table: DnsTable, _proxy_rules: ProxyRules) -> Result<()> {
    anyhow::bail!("netgate requires Unix/Linux")
}

#[cfg(unix)]
async fn control_plane(dns_table: DnsTable, proxy_rules: ProxyRules) -> Result<()> {
    let socket_path = format!("{SOCKET_DIR}/netgate.sock");
    let p = Path::new(&socket_path);
    if p.exists() { std::fs::remove_file(p)?; }
    if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
    let listener = UnixListener::bind(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660))?;
    info!("netgate control plane at {socket_path}");

    loop {
        let (mut stream, _) = listener.accept().await?;
        let dns_table = dns_table.clone();
        let proxy_rules = proxy_rules.clone();
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
                Ok(ControlOp::Custom { tag, payload: inner }) => match tag.as_str() {
                    "dns.upsert" => {
                        match bincode::serde::decode_from_slice::<DnsRecord, _>(
                            &inner, bincode::config::standard()
                        ) {
                            Ok((rec, _)) => {
                                let name = rec.name.clone();
                                dns_table.write().await.insert(name, rec);
                                ControlReply::Ok
                            }
                            Err(e) => ControlReply::Error { message: e.to_string() },
                        }
                    }
                    "dns.delete" => {
                        if let Ok(name) = std::str::from_utf8(&inner) {
                            dns_table.write().await.remove(name);
                        }
                        ControlReply::Ok
                    }
                    "proxy.add" => {
                        match bincode::serde::decode_from_slice::<ProxyRule, _>(
                            &inner, bincode::config::standard()
                        ) {
                            Ok((rule, _)) => {
                                let rule_clone = rule.clone();
                                proxy_rules.write().await.push(rule);
                                tokio::spawn(run_proxy_rule(rule_clone));
                                ControlReply::Ok
                            }
                            Err(e) => ControlReply::Error { message: e.to_string() },
                        }
                    }
                    _ => ControlReply::Error { message: format!("unknown tag: {tag}") },
                },
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
            .add_directive("netgate=info".parse()?))
        .init();

    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/hivebus/netgate.toml".into());
    let cfg: Config = if Path::new(&config_path).exists() {
        let raw = std::fs::read_to_string(&config_path)?;
        toml::from_str(&raw)?
    } else {
        toml::from_str("dns_addr = \"0.0.0.0:5353\"\ndomain = \"cluster.internal\"").unwrap()
    };

    let dns_table: DnsTable = Default::default();
    let proxy_rules: ProxyRules = Default::default();

    tokio::spawn(dns_server(dns_table.clone(), cfg.dns_addr));
    control_plane(dns_table, proxy_rules).await
}
