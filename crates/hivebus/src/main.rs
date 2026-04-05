//! hivebus — cluster heartbeat, discovery, and master election daemon.
//!
//! # Responsibilities
//! - Broadcast/multicast UDP frames on the cluster network
//! - Maintain a membership table: Alive / Suspect / Dead per node
//! - Elect a master node (bully: lowest node_id that is Alive wins)
//! - Expose cluster state over a Unix socket control plane
//!
//! # Frame protocol  (see `proto` crate)
//! ```text
//! [0xCA][type_seq][node_id_hi][node_id_lo][bincode payload...]
//! ```
//!
//! # Failure detection timing
//! - Heartbeat interval : 150 ms
//! - Suspect threshold  : 3 missed heartbeats (~450 ms)
//! - Dead threshold     : 30 s
//! - Cleanup threshold  : 5 min

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use proto::{
    AnnouncePayload, ControlOp, ControlReply, FrameHeader, GossipEntry, GossipPayload,
    HeartbeatPayload, LeavePayload, MsgType, NodeInfo, NodeState, CONTROL_FRAME_MARKER,
    DISCOVERY_PORT, MAX_CONTROL_PAYLOAD, MULTICAST_ADDR, SOCKET_DIR,
};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    /// Network interface to bind for cluster comms (e.g. "eth1").
    cluster_iface: String,
    /// IP address on that interface.
    cluster_addr: Ipv4Addr,
    /// Optional: override auto-derived node_id.
    node_id: Option<u16>,
    /// Human-readable hostname (defaults to system hostname).
    hostname: Option<String>,
    /// Heartbeat interval in milliseconds (default 150).
    #[serde(default = "default_hb_interval")]
    heartbeat_interval_ms: u64,
    /// How many missed heartbeats before marking Suspect (default 3).
    #[serde(default = "default_suspect_misses")]
    suspect_misses: u32,
    /// Seconds before a Suspect becomes Dead (default 30).
    #[serde(default = "default_dead_secs")]
    dead_secs: u64,
}

fn default_hb_interval() -> u64 { 150 }
fn default_suspect_misses() -> u32 { 3 }
fn default_dead_secs() -> u64 { 30 }

impl Config {
    fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {path}"))?;
        toml::from_str(&raw).context("parsing config")
    }
}

// ---------------------------------------------------------------------------
// Membership table
// ---------------------------------------------------------------------------

type Members = Arc<RwLock<HashMap<u16, NodeInfo>>>;

fn master_from(members: &HashMap<u16, NodeInfo>, self_id: u16) -> Option<u16> {
    // Simple bully: lowest node_id that is Alive (including self).
    let mut alive_ids: Vec<u16> = members
        .values()
        .filter(|n| n.state == NodeState::Alive)
        .map(|n| n.node_id)
        .collect();
    alive_ids.push(self_id);
    alive_ids.sort_unstable();
    alive_ids.into_iter().next()
}

// ---------------------------------------------------------------------------
// Network helpers
// ---------------------------------------------------------------------------

/// Build a UDP socket that can send broadcast AND join the multicast group,
/// bound to `bind_addr:DISCOVERY_PORT`.
fn build_udp_socket(bind_addr: Ipv4Addr, iface_addr: Ipv4Addr) -> Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    sock.set_reuse_port(true)?;
    sock.set_broadcast(true)?;
    let bind: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into();
    sock.bind(&bind.into())?;

    // Join multicast
    let mcast: Ipv4Addr = MULTICAST_ADDR.parse()?;
    sock.join_multicast_v4(&mcast, &iface_addr)?;
    sock.set_multicast_loop_v4(false)?;

    Ok(sock.into())
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn encode_frame(header: FrameHeader, payload: &impl Serialize) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&header.to_bytes());
    let encoded = bincode::serde::encode_to_vec(payload, bincode::config::standard())?;
    buf.extend_from_slice(&encoded);
    if buf.len() > 508 {
        anyhow::bail!("discovery frame too large: {} bytes", buf.len());
    }
    Ok(buf)
}

fn local_node_info(
    node_id: u16,
    hostname: &str,
    control_addr: SocketAddr,
    incarnation: &Arc<std::sync::atomic::AtomicU32>,
    is_master: bool,
) -> NodeInfo {
    NodeInfo {
        node_id,
        hostname: hostname.to_string(),
        control_addr,
        state: NodeState::Alive,
        incarnation: incarnation.load(std::sync::atomic::Ordering::Relaxed),
        last_seen: SystemTime::now(),
        is_master,
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// Sends a HEARTBEAT frame every `interval_ms` milliseconds.
async fn heartbeat_task(
    sock: Arc<UdpSocket>,
    node_id: u16,
    incarnation: Arc<std::sync::atomic::AtomicU32>,
    interval_ms: u64,
) {
    let mut seq: u8 = 0;
    let broadcast: SocketAddr = SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT).into();
    let mcast_addr: SocketAddr = format!("{MULTICAST_ADDR}:{DISCOVERY_PORT}").parse().unwrap();

    let mut tick = interval(Duration::from_millis(interval_ms));
    loop {
        tick.tick().await;
        let payload = HeartbeatPayload {
            incarnation: incarnation.load(std::sync::atomic::Ordering::Relaxed),
            epoch_ms: epoch_ms(),
        };
        let header = FrameHeader::new(MsgType::Heartbeat, seq, node_id);
        match encode_frame(header, &payload) {
            Ok(buf) => {
                let _ = sock.send_to(&buf, broadcast).await;
                let _ = sock.send_to(&buf, mcast_addr).await;
            }
            Err(e) => error!("heartbeat encode error: {e}"),
        }
        seq = seq.wrapping_add(1) & 0x0F;
    }
}

/// Receives UDP frames and updates the membership table.
async fn listener_task(sock: Arc<UdpSocket>, members: Members, self_id: u16) {
    let mut buf = [0u8; 576];
    loop {
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => { error!("recv error: {e}"); continue; }
        };
        let data = &buf[..n];

        let Some(header) = FrameHeader::from_bytes(data) else {
            debug!("invalid frame magic from {from}");
            continue;
        };
        let nid = header.node_id();
        if nid == self_id {
            // Own frame looped back — ignore.
            continue;
        }

        let payload_bytes = &data[4..];

        let Some(msg_type) = header.msg_type() else {
            debug!("unknown msg type from {from}");
            continue;
        };

        match msg_type {
            MsgType::Heartbeat => {
                if let Ok((pl, _)) = bincode::serde::decode_from_slice::<HeartbeatPayload, _>(
                    payload_bytes, bincode::config::standard(),
                ) {
                    let mut m = members.write().await;
                    let entry = m.entry(nid).or_insert_with(|| NodeInfo {
                        node_id: nid,
                        hostname: format!("node-{nid:#06x}"),
                        control_addr: from,
                        state: NodeState::Alive,
                        incarnation: pl.incarnation,
                        last_seen: SystemTime::now(),
                        is_master: false,
                    });
                    entry.state = NodeState::Alive;
                    entry.incarnation = pl.incarnation;
                    entry.last_seen = SystemTime::now();
                }
            }
            MsgType::Announce => {
                if let Ok((pl, _)) = bincode::serde::decode_from_slice::<AnnouncePayload, _>(
                    payload_bytes, bincode::config::standard(),
                ) {
                    info!(node_id = nid, hostname = %pl.hostname, "node announced");
                    let mut m = members.write().await;
                    if let Some(existing) = m.get(&nid) {
                        if existing.hostname != pl.hostname || existing.control_addr != pl.control_addr {
                            warn!(
                                node_id = nid,
                                previous_hostname = %existing.hostname,
                                previous_addr = %existing.control_addr,
                                new_hostname = %pl.hostname,
                                new_addr = %pl.control_addr,
                                "duplicate node_id announcement replaced existing peer"
                            );
                        }
                    }
                    m.insert(nid, NodeInfo {
                        node_id: nid,
                        hostname: pl.hostname,
                        control_addr: pl.control_addr,
                        state: NodeState::Alive,
                        incarnation: pl.incarnation,
                        last_seen: SystemTime::now(),
                        is_master: false,
                    });
                }
            }
            MsgType::Leave => {
                if let Ok((pl, _)) = bincode::serde::decode_from_slice::<LeavePayload, _>(
                    payload_bytes, bincode::config::standard(),
                ) {
                    info!(node_id = nid, reason = %pl.reason, "node leaving");
                    let mut m = members.write().await;
                    if let Some(e) = m.get_mut(&nid) {
                        e.state = NodeState::Dead;
                    }
                }
            }
            MsgType::Gossip => {
                if let Ok((pl, _)) = bincode::serde::decode_from_slice::<GossipPayload, _>(
                    payload_bytes, bincode::config::standard(),
                ) {
                    let mut m = members.write().await;
                    for entry in pl.entries {
                        if entry.node_id == self_id { continue; }
                        let e = m.entry(entry.node_id).or_insert_with(|| NodeInfo {
                            node_id: entry.node_id,
                            hostname: format!("node-{:#06x}", entry.node_id),
                            control_addr: from,
                            state: entry.state,
                            incarnation: entry.incarnation,
                            last_seen: SystemTime::now(),
                            is_master: false,
                        });
                        // Only update if gossip carries newer incarnation.
                        if entry.incarnation >= e.incarnation {
                            e.state = entry.state;
                            e.incarnation = entry.incarnation;
                        }
                    }
                }
            }
            // VoteRequest / VoteGrant handled by election module (future).
            MsgType::VoteRequest | MsgType::VoteGrant | MsgType::Ack => {}
        }
    }
}

/// Scans the membership table, marks Suspect/Dead, removes stale entries,
/// and recalculates the master.
async fn failure_detector_task(
    members: Members,
    self_id: u16,
    suspect_threshold: Duration,
    dead_threshold: Duration,
) {
    let cleanup_threshold = Duration::from_secs(300);
    let mut tick = interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let now = SystemTime::now();
        let mut m = members.write().await;

        m.retain(|_id, node| {
            let age = now.duration_since(node.last_seen).unwrap_or(Duration::MAX);
            age < cleanup_threshold
        });

        for node in m.values_mut() {
            let age = now.duration_since(node.last_seen).unwrap_or(Duration::MAX);
            if age > dead_threshold {
                if node.state != NodeState::Dead {
                    warn!(node_id = node.node_id, "node declared Dead");
                }
                node.state = NodeState::Dead;
            } else if age > suspect_threshold && node.state == NodeState::Alive {
                warn!(node_id = node.node_id, "node Suspected");
                node.state = NodeState::Suspect;
            }
        }

        // Recompute master flag.
        let master_id = master_from(&m, self_id);
        for node in m.values_mut() {
            node.is_master = Some(node.node_id) == master_id;
        }
    }
}

/// Sends a GOSSIP frame every 500 ms carrying the current membership view.
async fn gossip_task(
    sock: Arc<UdpSocket>,
    members: Members,
    node_id: u16,
    incarnation: Arc<std::sync::atomic::AtomicU32>,
) {
    let mcast_addr: SocketAddr = format!("{MULTICAST_ADDR}:{DISCOVERY_PORT}").parse().unwrap();
    let mut seq: u8 = 0;
    let mut tick = interval(Duration::from_millis(500));
    loop {
        tick.tick().await;
        let entries: Vec<GossipEntry> = {
            let m = members.read().await;
            m.values()
                .map(|n| GossipEntry {
                    node_id: n.node_id,
                    incarnation: n.incarnation,
                    state: n.state,
                    epoch_ms: epoch_ms(),
                })
                .collect()
        };
        if entries.is_empty() { continue; }
        let payload = GossipPayload { entries };
        let header = FrameHeader::new(MsgType::Gossip, seq, node_id);
        if let Ok(buf) = encode_frame(header, &payload) {
            let _ = sock.send_to(&buf, mcast_addr).await;
        }
        seq = seq.wrapping_add(1) & 0x0F;
    }
}

#[cfg(not(unix))]
async fn control_plane_task(
    _members: Members,
    _self_id: u16,
    _self_hostname: String,
    _self_control_addr: SocketAddr,
    _incarnation: Arc<std::sync::atomic::AtomicU32>,
    _socket_path: String,
) -> Result<()> {
    anyhow::bail!("hivebus requires Unix/Linux")
}

/// Serves the Unix socket control plane.
#[cfg(unix)]
async fn control_plane_task(
    members: Members,
    self_id: u16,
    self_hostname: String,
    self_control_addr: SocketAddr,
    incarnation: Arc<std::sync::atomic::AtomicU32>,
    socket_path: String,
) -> Result<()> {
    let p = Path::new(&socket_path);
    if p.exists() { std::fs::remove_file(p)?; }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(p)?;
    // Only root/hivebus group may access.
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660))?;
    info!("control plane listening on {socket_path}");

    loop {
        let (mut stream, _) = listener.accept().await?;
        let members = members.clone();
        let self_hostname = self_hostname.clone();
        let incarnation = incarnation.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut len_buf = [0u8; 3];
            if stream.read_exact(&mut len_buf).await.is_err() { return; }
            if len_buf[0] != CONTROL_FRAME_MARKER {
                warn!(marker = len_buf[0], "invalid control frame marker");
                return;
            }
            let payload_len = u16::from_be_bytes([len_buf[1], len_buf[2]]) as usize;
            if payload_len > MAX_CONTROL_PAYLOAD {
                warn!(payload_len, "control frame exceeded max payload");
                return;
            }
            let mut payload = vec![0u8; payload_len];
            if stream.read_exact(&mut payload).await.is_err() { return; }

            let reply = match proto::decode_control::<ControlOp>(&payload) {
                Ok(ControlOp::Ping) => ControlReply::Ok,
                Ok(ControlOp::GetNodes) => {
                    let m = members.read().await;
                    let master_id = master_from(&m, self_id);
                    let mut nodes: Vec<NodeInfo> = m.values().cloned().collect();
                    nodes.push(local_node_info(
                        self_id,
                        &self_hostname,
                        self_control_addr,
                        &incarnation,
                        master_id == Some(self_id),
                    ));
                    nodes.sort_unstable_by_key(|node| node.node_id);
                    ControlReply::Nodes(nodes)
                }
                Ok(ControlOp::GetMaster) => {
                    let m = members.read().await;
                    let master_id = master_from(&m, self_id);
                    let master = master_id.and_then(|id| {
                        if id == self_id {
                            Some(local_node_info(
                                self_id,
                                &self_hostname,
                                self_control_addr,
                                &incarnation,
                                true,
                            ))
                        } else {
                            m.get(&id).cloned()
                        }
                    });
                    ControlReply::Master(master)
                }
                Ok(ControlOp::Shutdown) => {
                    info!("shutdown requested via control socket");
                    std::process::exit(0);
                }
                Err(e) => ControlReply::Error { message: e.to_string() },
                _ => ControlReply::Error { message: "unsupported op".into() },
            };

            if let Ok(encoded) = proto::encode_control(&reply) {
                let _ = stream.write_all(&encoded).await;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("hivebus=info".parse()?),
        )
        .json()
        .init();

    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/hivebus/hivebus.toml".into());
    let cfg = Config::load(&config_path)?;

    let hostname = cfg.hostname.clone().unwrap_or_else(|| {
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".into())
    });

    // Derive or use configured node ID.
    let node_id: u16 = if let Some(id) = cfg.node_id {
        id
    } else {
        // Derive from primary MAC of cluster interface.
        derive_node_id(&cfg.cluster_iface)?
    };

    info!(node_id, hostname, "hivebus starting");

    let incarnation = Arc::new(std::sync::atomic::AtomicU32::new(
        epoch_ms() as u32,
    ));

    // Build and wrap the UDP socket.
    let std_sock = build_udp_socket(cfg.cluster_addr, cfg.cluster_addr)?;
    std_sock.set_nonblocking(true)?;
    let udp = Arc::new(UdpSocket::from_std(std_sock)?);

    // Membership table.
    let members: Members = Arc::new(RwLock::new(HashMap::new()));

    let suspect_threshold = Duration::from_millis(
        cfg.heartbeat_interval_ms * cfg.suspect_misses as u64,
    );
    let dead_threshold = Duration::from_secs(cfg.dead_secs);

    let socket_path = format!("{SOCKET_DIR}/hivebus.sock");
    let control_addr: SocketAddr = format!("{}:{}", cfg.cluster_addr, DISCOVERY_PORT + 1)
        .parse()?;

    // Send initial ANNOUNCE.
    {
        let payload = AnnouncePayload {
            incarnation: incarnation.load(std::sync::atomic::Ordering::Relaxed),
            hostname: hostname.clone(),
            control_addr,
            version: env!("CARGO_PKG_VERSION").into(),
        };
        let header = FrameHeader::new(MsgType::Announce, 0, node_id);
        if let Ok(buf) = encode_frame(header, &payload) {
            let broadcast: SocketAddr = SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT).into();
            let _ = udp.send_to(&buf, broadcast).await;
            let mcast: SocketAddr = format!("{MULTICAST_ADDR}:{DISCOVERY_PORT}").parse()?;
            let _ = udp.send_to(&buf, mcast).await;
        }
    }

    // Spawn tasks.
    tokio::spawn(heartbeat_task(
        udp.clone(),
        node_id,
        incarnation.clone(),
        cfg.heartbeat_interval_ms,
    ));
    tokio::spawn(listener_task(
        udp.clone(),
        members.clone(),
        node_id,
    ));
    tokio::spawn(failure_detector_task(
        members.clone(),
        node_id,
        suspect_threshold,
        dead_threshold,
    ));
    tokio::spawn(gossip_task(
        udp.clone(),
        members.clone(),
        node_id,
        incarnation.clone(),
    ));

    // Control plane (blocks on Accept loop).
    control_plane_task(
        members.clone(),
        node_id,
        hostname.clone(),
        control_addr,
        incarnation.clone(),
        socket_path,
    ).await?;
    Ok(())
}

/// Read the MAC address of `iface` from sysfs and derive a 16-bit node ID.
fn derive_node_id(iface: &str) -> Result<u16> {
    let path = format!("/sys/class/net/{iface}/address");
    let mac_str = std::fs::read_to_string(&path)
        .with_context(|| format!("reading MAC from {path}"))?;
    let mac_str = mac_str.trim();
    let octets: Vec<u8> = mac_str
        .split(':')
        .map(|s| u8::from_str_radix(s, 16))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing MAC {mac_str}"))?;
    if octets.len() != 6 {
        anyhow::bail!("unexpected MAC length: {mac_str}");
    }
    let mac: [u8; 6] = octets.try_into().unwrap();
    Ok(proto::node_id_from_mac(&mac))
}

// hostname helper
#[cfg(unix)]
mod hostname {
    use std::ffi::OsString;
    pub fn get() -> std::io::Result<OsString> {
        let mut buf = [0i8; 256];
        let ret = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        Ok(OsString::from(cstr.to_string_lossy().as_ref()))
    }
}

#[cfg(not(unix))]
mod hostname {
    use std::ffi::OsString;
    pub fn get() -> std::io::Result<OsString> {
        Ok(OsString::from(
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
        ))
    }
}

#[cfg(unix)]
extern crate libc;
