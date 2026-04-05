//! pxeboot — PXE/iPXE network boot manager
//!
//! Provides DHCP + TFTP for bare-metal nodes, enabling iPXE chainload.
//!
//! # Flow
//! 1. Node broadcasts DHCP DISCOVER
//! 2. pxeboot replies with OFFER:
//!    - Option 66 (TFTP server IP)
//!    - Option 67 (boot filename, e.g. `ipxe.efi`)
//! 3. Node downloads iPXE via TFTP
//! 4. iPXE executes per-MAC script (served as TFTP file `scripts/<mac>.ipxe`)
//! 5. pxeboot registers new MAC in hivebus via AnnouncePayload
//!
//! # DHCP server
//! Custom implementation (~800 LOC) using raw UDP socket bound to :67.
//! Parses DHCP messages (RFC 2132), manages leases, encodes PXE options.
//!
//! # TFTP server
//! Built-in read-only TFTP implementation that serves bootloaders plus
//! generated per-MAC iPXE scripts from `tftp_root`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::net::UdpSocket;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tracing::{error, info, warn};

use proto::{ControlOp, ControlReply, SOCKET_DIR};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    /// IP address of this server (used as DHCP `siaddr` / TFTP server IP).
    server_ip: Ipv4Addr,
    /// Network interface to listen on (e.g. "eth0").
    iface: String,
    /// DHCP pool start.
    pool_start: Ipv4Addr,
    /// DHCP pool end.
    pool_end: Ipv4Addr,
    /// Subnet mask.
    #[serde(default = "default_mask")]
    subnet_mask: Ipv4Addr,
    /// Default gateway for booting nodes.
    gateway: Option<Ipv4Addr>,
    /// Root directory for TFTP files.
    #[serde(default = "default_tftp_root")]
    tftp_root: String,
    /// Default iPXE boot filename (e.g. "ipxe.efi").
    #[serde(default = "default_boot_file")]
    boot_file: String,
    /// Future hand-off URL for HTTP-based bootstrap payloads.
    seed_url: Option<String>,
    /// DHCP lease duration in seconds.
    #[serde(default = "default_lease_secs")]
    lease_secs: u32,
}
fn default_mask() -> Ipv4Addr { "255.255.255.0".parse().unwrap() }
fn default_tftp_root() -> String { "/var/lib/hivebus/tftp".into() }
fn default_boot_file() -> String { "ipxe.efi".into() }
fn default_lease_secs() -> u32 { 3600 }

impl Config {
    fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("pxeboot requires config at {path}"))?;
        let cfg = toml::from_str::<Config>(&raw).context("parsing pxeboot config")?;
        if u32::from(cfg.pool_start) > u32::from(cfg.pool_end) {
            anyhow::bail!(
                "invalid DHCP pool: pool_start {} is after pool_end {}",
                cfg.pool_start,
                cfg.pool_end
            );
        }
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// Boot config per MAC
// ---------------------------------------------------------------------------

/// 6-byte MAC address.
pub type Mac = [u8; 6];

fn fmt_mac(mac: &Mac) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn default_script_path(mac: &Mac) -> String {
    format!("scripts/{}.ipxe", fmt_mac(mac))
}

fn sanitize_tftp_path(root: &Path, requested: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(root);
    for component in Path::new(requested).components() {
        match component {
            Component::Normal(segment) => path.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

fn render_ipxe_script(cfg: &Config, mac: &Mac, entry: &BootEntry) -> String {
    let seed_url = entry
        .tags
        .get("seed_url")
        .cloned()
        .or_else(|| cfg.seed_url.clone())
        .unwrap_or_else(|| format!("http://{}:7780/seed.tar.gz", cfg.server_ip));

    format!(
        "#!ipxe\n\
echo hivebus bootstrap for {} ({})\n\
set seed-url {}\n\
chain {}\n",
        entry.hostname,
        fmt_mac(mac),
        seed_url,
        seed_url,
    )
}

async fn write_ipxe_script(cfg: &Config, mac: &Mac, entry: &BootEntry) -> Result<()> {
    let Some(rel_path) = entry.ipxe_script.as_ref() else {
        return Ok(());
    };

    let path = sanitize_tftp_path(Path::new(&cfg.tftp_root), rel_path)
        .context("invalid iPXE script path")?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, render_ipxe_script(cfg, mac, entry)).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootEntry {
    /// Human-readable hostname to assign.
    pub hostname: String,
    /// Static IP override (if None, assign from pool).
    pub ip: Option<Ipv4Addr>,
    /// Custom iPXE script path under tftp_root (if None, serve default.ipxe).
    pub ipxe_script: Option<String>,
    /// Arbitrary key-value metadata.
    pub tags: HashMap<String, String>,
}

type BootTable = Arc<RwLock<HashMap<Mac, BootEntry>>>;

// ---------------------------------------------------------------------------
// DHCP constants (RFC 2132)
// ---------------------------------------------------------------------------

const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_TFTP_SERVER: u8 = 66;
const OPT_BOOT_FILE: u8 = 67;
const OPT_END: u8 = 255;

const DHCPDISCOVER: u8 = 1;
const DHCPOFFER: u8 = 2;
const DHCPREQUEST: u8 = 3;
const DHCPACK: u8 = 5;

const TFTP_RRQ: u16 = 1;
const TFTP_DATA: u16 = 3;
const TFTP_ACK: u16 = 4;
const TFTP_ERROR: u16 = 5;
const TFTP_BLOCK_SIZE: usize = 512;
const TFTP_MAX_RETRIES: usize = 5;
const TFTP_ACK_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// DHCP lease table
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Lease {
    ip: Ipv4Addr,
    expires: std::time::Instant,
}

type LeaseTable = Arc<tokio::sync::Mutex<HashMap<Mac, Lease>>>;

async fn allocate_ip(
    mac: &Mac,
    boot_table: &BootTable,
    leases: &LeaseTable,
    pool_start: Ipv4Addr,
    pool_end: Ipv4Addr,
    lease_secs: u32,
) -> Option<Ipv4Addr> {
    // Check for static assignment first.
    if let Some(entry) = boot_table.read().await.get(mac) {
        if let Some(ip) = entry.ip {
            return Some(ip);
        }
    }
    // Reuse existing lease.
    {
        let mut leases = leases.lock().await;
        if let Some(lease) = leases.get_mut(mac) {
            lease.expires = std::time::Instant::now()
                + std::time::Duration::from_secs(lease_secs as u64);
            return Some(lease.ip);
        }
    }
    // Allocate next free IP.
    let start = u32::from(pool_start);
    let end = u32::from(pool_end);
    let leases = leases.lock().await;
    let in_use: std::collections::HashSet<u32> =
        leases.values().map(|l| u32::from(l.ip)).collect();
    (start..=end).find(|ip| !in_use.contains(ip)).map(Ipv4Addr::from)
}

// ---------------------------------------------------------------------------
// DHCP server
// ---------------------------------------------------------------------------

/// Minimal DHCP packet builder (OFFER / ACK).
fn build_dhcp_reply(
    op: u8,           // DHCPOFFER or DHCPACK
    xid: u32,
    chaddr: &[u8],    // client MAC (16 bytes with padding)
    yiaddr: Ipv4Addr, // offered IP
    siaddr: Ipv4Addr, // server IP (also TFTP server)
    subnet: Ipv4Addr,
    gateway: Option<Ipv4Addr>,
    lease_secs: u32,
    boot_file: &str,
) -> Vec<u8> {
    let mut pkt = vec![0u8; 236];
    pkt[0] = 2;            // BOOTREPLY
    pkt[1] = 1;            // htype = ethernet
    pkt[2] = 6;            // hlen
    pkt[3] = 0;            // hops
    pkt[4..8].copy_from_slice(&xid.to_be_bytes());
    // yiaddr
    pkt[16..20].copy_from_slice(&yiaddr.octets());
    // siaddr
    pkt[20..24].copy_from_slice(&siaddr.octets());
    // chaddr
    let copy_len = chaddr.len().min(16);
    pkt[28..28+copy_len].copy_from_slice(&chaddr[..copy_len]);
    // BOOTP legacy bootfile field for clients that prefer it over option 67.
    let boot_file_bytes = boot_file.as_bytes();
    let boot_copy_len = boot_file_bytes.len().min(127);
    pkt[108..108 + boot_copy_len].copy_from_slice(&boot_file_bytes[..boot_copy_len]);

    // DHCP magic cookie
    pkt.extend_from_slice(&DHCP_MAGIC);

    // Options
    pkt.extend_from_slice(&[OPT_MESSAGE_TYPE, 1, op]);
    pkt.extend_from_slice(&[OPT_SERVER_ID, 4]);
    pkt.extend_from_slice(&siaddr.octets());
    pkt.extend_from_slice(&[OPT_SUBNET_MASK, 4]);
    pkt.extend_from_slice(&subnet.octets());
    if let Some(gw) = gateway {
        pkt.extend_from_slice(&[OPT_ROUTER, 4]);
        pkt.extend_from_slice(&gw.octets());
    }
    pkt.extend_from_slice(&[OPT_LEASE_TIME, 4]);
    pkt.extend_from_slice(&lease_secs.to_be_bytes());
    // TFTP server IP (opt 66)
    pkt.push(OPT_TFTP_SERVER);
    pkt.push(4);
    pkt.extend_from_slice(&siaddr.octets());
    // Boot file (opt 67)
    pkt.push(OPT_BOOT_FILE);
    pkt.push(boot_copy_len as u8);
    pkt.extend_from_slice(&boot_file_bytes[..boot_copy_len]);
    pkt.push(OPT_END);
    pkt
}

async fn dhcp_server(cfg: Arc<Config>, boot_table: BootTable, leases: LeaseTable) {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::SocketAddr;

    let sock = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => { error!("DHCP socket create failed: {e}"); return; }
    };
    let _ = sock.set_reuse_address(true);
    #[cfg(target_os = "linux")]
    let _ = sock.set_reuse_port(true);
    let _ = sock.set_broadcast(true);
    let bind: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DHCP_SERVER_PORT).into();
    if let Err(e) = sock.bind(&bind.into()) {
        error!("DHCP bind :67 failed: {e} (are you root?)");
        return;
    }
    sock.set_nonblocking(true).unwrap();
    let std_sock: std::net::UdpSocket = sock.into();
    let udp = match tokio::net::UdpSocket::from_std(std_sock) {
        Ok(s) => s,
        Err(e) => { error!("DHCP tokio socket: {e}"); return; }
    };

    info!("DHCP server listening on :67");
    let mut buf = [0u8; 576];
    loop {
        let (n, _) = match udp.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => { error!("DHCP recv: {e}"); continue; }
        };
        let data = &buf[..n];
        if n < 240 || data[236..240] != DHCP_MAGIC { continue; }

        let xid = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let flags = u16::from_be_bytes([data[10], data[11]]);
        let chaddr: [u8; 16] = data[28..44].try_into().unwrap_or([0u8; 16]);
        let mac: Mac = chaddr[..6].try_into().unwrap();

        // Find DHCP message type option.
        let opts = &data[240..];
        let mut msg_type = 0u8;
        let mut i = 0;
        while i < opts.len() {
            let opt = opts[i];
            if opt == OPT_END { break; }
            if opt == 0 { i += 1; continue; } // pad
            if i + 1 >= opts.len() { break; }
            let len = opts[i + 1] as usize;
            if opt == OPT_MESSAGE_TYPE && len >= 1 { msg_type = opts[i + 2]; }
            i += 2 + len;
        }

        if msg_type == DHCPDISCOVER || msg_type == DHCPREQUEST {
            let mac_str = fmt_mac(&mac);
            info!(mac = %mac_str, msg_type, "DHCP request");

            // Register unknown MAC in boot table and materialize its iPXE script.
            let entry = {
                let mut bt = boot_table.write().await;
                bt.entry(mac).or_insert_with(|| {
                    info!(mac = %mac_str, "new hardware discovered via DHCP");
                    BootEntry {
                        hostname: format!("node-{mac_str}"),
                        ip: None,
                        ipxe_script: Some(default_script_path(&mac)),
                        tags: Default::default(),
                    }
                }).clone()
            };

            if let Err(e) = write_ipxe_script(cfg.as_ref(), &mac, &entry).await {
                warn!(mac = %mac_str, "failed to write iPXE script: {e}");
            }

            let Some(offered_ip) = allocate_ip(
                &mac, &boot_table, &leases,
                cfg.pool_start, cfg.pool_end, cfg.lease_secs,
            ).await else {
                warn!(mac = %mac_str, "DHCP pool exhausted");
                continue;
            };

            // Store lease.
            leases.lock().await.insert(mac, Lease {
                ip: offered_ip,
                expires: std::time::Instant::now()
                    + std::time::Duration::from_secs(cfg.lease_secs as u64),
            });

            // Determine boot file (per-MAC script or default).
            let boot_file = {
                let bt = boot_table.read().await;
                bt.get(&mac)
                  .and_then(|e| e.ipxe_script.clone())
                  .unwrap_or_else(|| cfg.boot_file.clone())
            };

            let reply_op = if msg_type == DHCPDISCOVER { DHCPOFFER } else { DHCPACK };
            let reply = build_dhcp_reply(
                reply_op, xid, &chaddr, offered_ip,
                cfg.server_ip, cfg.subnet_mask, cfg.gateway,
                cfg.lease_secs, &boot_file,
            );

            let target: SocketAddr = if (flags & 0x8000) != 0 {
                SocketAddrV4::new(Ipv4Addr::BROADCAST, DHCP_CLIENT_PORT).into()
            } else {
                SocketAddrV4::new(offered_ip, DHCP_CLIENT_PORT).into()
            };
            if let Err(e) = udp.send_to(&reply, target).await {
                error!("DHCP reply send: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TFTP server
// ---------------------------------------------------------------------------

fn parse_tftp_rrq(packet: &[u8]) -> Option<(String, String)> {
    if packet.len() < 4 || u16::from_be_bytes([packet[0], packet[1]]) != TFTP_RRQ {
        return None;
    }

    let payload = &packet[2..];
    let file_end = payload.iter().position(|b| *b == 0)?;
    let mode_start = file_end + 1;
    let mode_end = payload.get(mode_start..)?.iter().position(|b| *b == 0)? + mode_start;

    let filename = std::str::from_utf8(&payload[..file_end]).ok()?.to_string();
    let mode = std::str::from_utf8(&payload[mode_start..mode_end]).ok()?.to_ascii_lowercase();
    Some((filename, mode))
}

fn build_tftp_data(block: u16, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(4 + data.len());
    packet.extend_from_slice(&TFTP_DATA.to_be_bytes());
    packet.extend_from_slice(&block.to_be_bytes());
    packet.extend_from_slice(data);
    packet
}

fn build_tftp_error(code: u16, message: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(5 + message.len());
    packet.extend_from_slice(&TFTP_ERROR.to_be_bytes());
    packet.extend_from_slice(&code.to_be_bytes());
    packet.extend_from_slice(message.as_bytes());
    packet.push(0);
    packet
}

async fn serve_tftp_read(bind_addr: SocketAddr, root: PathBuf, requested: String, client: SocketAddr) {
    use tokio::io::AsyncReadExt;

    let Some(path) = sanitize_tftp_path(&root, &requested) else {
        let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await else {
            return;
        };
        let _ = sock.send_to(&build_tftp_error(2, "invalid path"), client).await;
        return;
    };

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => {
            warn!(path = %path.display(), client = %client, "TFTP open failed: {e}");
            let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await else {
                return;
            };
            let _ = sock.send_to(&build_tftp_error(1, "file not found"), client).await;
            return;
        }
    };

    let bind_ip = match bind_addr {
        SocketAddr::V4(addr) => *addr.ip(),
        SocketAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
    };
    let transfer_sock = match UdpSocket::bind(SocketAddrV4::new(bind_ip, 0)).await {
        Ok(sock) => sock,
        Err(e) => {
            error!(client = %client, "TFTP data socket bind failed: {e}");
            return;
        }
    };

    let mut block: u16 = 1;
    let mut data_buf = [0u8; TFTP_BLOCK_SIZE];
    loop {
        let bytes_read = match file.read(&mut data_buf).await {
            Ok(n) => n,
            Err(e) => {
                error!(path = %path.display(), client = %client, "TFTP read failed: {e}");
                let _ = transfer_sock.send_to(&build_tftp_error(0, "read failed"), client).await;
                return;
            }
        };

        let packet = build_tftp_data(block, &data_buf[..bytes_read]);
        let mut retries = 0;
        loop {
            if let Err(e) = transfer_sock.send_to(&packet, client).await {
                error!(client = %client, "TFTP send failed: {e}");
                return;
            }

            let mut ack_buf = [0u8; 4];
            match timeout(TFTP_ACK_TIMEOUT, transfer_sock.recv_from(&mut ack_buf)).await {
                Ok(Ok((4, from))) if from == client => {
                    let opcode = u16::from_be_bytes([ack_buf[0], ack_buf[1]]);
                    let ack_block = u16::from_be_bytes([ack_buf[2], ack_buf[3]]);
                    if opcode == TFTP_ACK && ack_block == block {
                        break;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn!(client = %client, "TFTP recv failed: {e}"),
                Err(_) => {}
            }

            retries += 1;
            if retries >= TFTP_MAX_RETRIES {
                warn!(client = %client, block, "TFTP transfer failed after retries");
                return;
            }
        }

        if bytes_read < TFTP_BLOCK_SIZE {
            info!(path = %path.display(), client = %client, blocks = block, "TFTP transfer complete");
            return;
        }

        block = block.wrapping_add(1);
    }
}

async fn tftp_server(tftp_root: PathBuf, bind_addr: SocketAddr) {
    let sock = match UdpSocket::bind(bind_addr).await {
        Ok(sock) => sock,
        Err(e) => {
            error!(%bind_addr, "TFTP bind failed: {e}");
            return;
        }
    };
    info!(%bind_addr, root = %tftp_root.display(), "TFTP server started");

    let mut buf = [0u8; 1500];
    loop {
        let (n, client) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                error!("TFTP recv failed: {e}");
                continue;
            }
        };
        let packet = &buf[..n];
        let Some((requested, mode)) = parse_tftp_rrq(packet) else {
            let _ = sock.send_to(&build_tftp_error(4, "unsupported operation"), client).await;
            continue;
        };
        if mode != "octet" && mode != "netascii" {
            let _ = sock.send_to(&build_tftp_error(0, "unsupported transfer mode"), client).await;
            continue;
        }
        tokio::spawn(serve_tftp_read(bind_addr, tftp_root.clone(), requested, client));
    }
}

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
async fn control_plane(_cfg: Arc<Config>, _boot_table: BootTable) -> Result<()> {
    anyhow::bail!("pxeboot control plane requires Unix/Linux")
}

#[cfg(unix)]
async fn control_plane(cfg: Arc<Config>, boot_table: BootTable) -> Result<()> {
    let socket_path = format!("{SOCKET_DIR}/pxeboot.sock");
    let p = Path::new(&socket_path);
    if p.exists() { std::fs::remove_file(p)?; }
    if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
    let listener = UnixListener::bind(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660))?;
    info!("pxeboot control plane at {socket_path}");

    loop {
        let (mut stream, _) = listener.accept().await?;
        let cfg = cfg.clone();
        let boot_table = boot_table.clone();
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
                    "boot.register" => {
                        // payload: (Mac, BootEntry)
                        match bincode::serde::decode_from_slice::<(Mac, BootEntry), _>(
                            &inner, bincode::config::standard()
                        ) {
                            Ok(((mac, entry), _)) => {
                                info!(mac = %fmt_mac(&mac), hostname = %entry.hostname, "boot entry registered");
                                boot_table.write().await.insert(mac, entry.clone());
                                if let Err(e) = write_ipxe_script(cfg.as_ref(), &mac, &entry).await {
                                    warn!(mac = %fmt_mac(&mac), "failed to write iPXE script: {e}");
                                }
                                ControlReply::Ok
                            }
                            Err(e) => ControlReply::Error { message: e.to_string() },
                        }
                    }
                    "boot.list" => {
                        let bt = boot_table.read().await;
                        let entries: Vec<(String, BootEntry)> = bt
                            .iter()
                            .map(|(mac, e)| (fmt_mac(mac), e.clone()))
                            .collect();
                        match bincode::serde::encode_to_vec(&entries, bincode::config::standard()) {
                            Ok(data) => ControlReply::Data { tag: "boot.list".into(), payload: data },
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
            .add_directive("pxeboot=info".parse()?))
        .init();

    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/hivebus/pxeboot.toml".into());
    let cfg = Arc::new(Config::load(&config_path)?);

    std::fs::create_dir_all(&cfg.tftp_root)?;
    std::fs::create_dir_all(Path::new(&cfg.tftp_root).join("scripts"))?;

    let boot_table: BootTable = Default::default();
    let leases: LeaseTable = Default::default();
    let tftp_bind: std::net::SocketAddr =
        format!("{}:69", cfg.server_ip).parse()?;

    tokio::spawn(dhcp_server(cfg.clone(), boot_table.clone(), leases));
    tokio::spawn(tftp_server(PathBuf::from(&cfg.tftp_root), tftp_bind));
    control_plane(cfg, boot_table).await
}
