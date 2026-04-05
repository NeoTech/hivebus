#!/usr/bin/env bash
# provision-agent.sh — agent-node (node2/3) bootstrapper, runs as root.
#
# Flow:
#   1. Install minimal runtime packages (no Rust, no compiler).
#   2. Wait for pxeboot on node1 to assign a DHCP lease on eth1.
#   3. Read the eth1 MAC address.
#   4. Derive NODE_ID using the same XOR fold as proto::node_id_from_mac:
#        hi = mac[0] ^ mac[2] ^ mac[4]
#        lo = mac[1] ^ mac[3] ^ mac[5]
#        node_id = (hi << 8) | lo   (1..=65534, clamped to 2..255 for display)
#   5. Pull the per-MAC iPXE script from pxeboot via TFTP and use it to
#      discover the HTTP seed archive URL.
#   6. Download seed.tar.gz and extract sc-* binaries to /usr/local/bin/.
#   7. Write /etc/hivebus/hivebus.toml, create systemd unit, start hivebus.
#      hivebus broadcasts an ANNOUNCE — node1 logs "new hardware discovered".
#
# Environment variables set by Vagrantfile:
#   SEED_IP        10.42.0.11
#   CLUSTER_IFACE  eth1

set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

SEED_IP="${SEED_IP:-10.42.0.11}"
CLUSTER_IFACE="${CLUSTER_IFACE:-eth1}"
SEED_URL="http://${SEED_IP}:7780/seed.tar.gz"

log() { echo "[agent-provision] $*"; }

# ---------------------------------------------------------------------------
# 1. Minimal runtime packages
# ---------------------------------------------------------------------------
log "Installing runtime packages..."
apt-get update -qq
apt-get install -y -qq \
    curl \
    isc-dhcp-client \
    iproute2 \
    nftables \
    socat \
    tftp-hpa \
    jq \
    netcat-openbsd

systemctl enable nftables || true

# ---------------------------------------------------------------------------
# 2. Runtime directories
# ---------------------------------------------------------------------------
install -d -m 755 /etc/hivebus
install -d -m 755 /var/run/hivebus
install -d -m 755 /var/lib/hivebus/images
install -d -m 755 /var/lib/hivebus/tftp
install -d -m 755 /var/lib/hivebus/lxc

# ---------------------------------------------------------------------------
# 3. Wait for DHCP lease on cluster interface
# ---------------------------------------------------------------------------
log "Waiting for DHCP lease on ${CLUSTER_IFACE} from ${SEED_IP} pxeboot..."
dhclient -v "${CLUSTER_IFACE}" || true
CLUSTER_IP=""
for i in $(seq 1 60); do
    CLUSTER_IP=$(ip -4 addr show "${CLUSTER_IFACE}" 2>/dev/null \
        | awk '/inet / { split($2,a,"/"); print a[1]; exit }')
    if [ -n "${CLUSTER_IP}" ]; then
        log "Got IP: ${CLUSTER_IP}"
        break
    fi
    sleep 2
done

if [ -z "${CLUSTER_IP}" ]; then
    log "ERROR: no DHCP lease on ${CLUSTER_IFACE} after 120s — is node1 up and pxeboot running?"
    exit 1
fi

# ---------------------------------------------------------------------------
# 4. Derive NODE_ID from MAC address (mirrors proto::node_id_from_mac)
# ---------------------------------------------------------------------------
MAC_RAW=$(cat "/sys/class/net/${CLUSTER_IFACE}/address" 2>/dev/null || true)
if [ -z "${MAC_RAW}" ]; then
    log "ERROR: could not read MAC from /sys/class/net/${CLUSTER_IFACE}/address"
    exit 1
fi

log "eth1 MAC: ${MAC_RAW}"

# Parse hex bytes from MAC (format: aa:bb:cc:dd:ee:ff)
IFS=':' read -r -a OCTETS <<< "${MAC_RAW}"
m0=$((16#${OCTETS[0]}))
m1=$((16#${OCTETS[1]}))
m2=$((16#${OCTETS[2]}))
m3=$((16#${OCTETS[3]}))
m4=$((16#${OCTETS[4]}))
m5=$((16#${OCTETS[5]}))

HI=$(( (m0 ^ m2 ^ m4) & 0xFF ))
LO=$(( (m1 ^ m3 ^ m5) & 0xFF ))
NODE_ID_RAW=$(( (HI << 8) | LO ))

# Map to a non-zero u8-range display ID (2–254); use modulo + 2 to stay
# readable.  The actual wire node_id remains the full 16-bit value.
DISPLAY_ID=$(( (NODE_ID_RAW % 253) + 2 ))
NODE_ID="${NODE_ID_RAW}"

log "Derived NODE_ID: ${NODE_ID}  (display: node${DISPLAY_ID})"
HOSTNAME="node${DISPLAY_ID}"
hostnamectl set-hostname "${HOSTNAME}" || true

# ---------------------------------------------------------------------------
# 5. Fetch pxeboot-generated iPXE script and derive the seed URL
# ---------------------------------------------------------------------------
SCRIPT_TMP=$(mktemp)
SCRIPT_REMOTE="scripts/${MAC_RAW}.ipxe"
log "Fetching bootstrap script via TFTP: ${SCRIPT_REMOTE}"
if timeout 10 tftp "${SEED_IP}" -m binary -c get "${SCRIPT_REMOTE}" "${SCRIPT_TMP}" >/dev/null 2>&1; then
    SCRIPT_SEED_URL=$(awk '/^chain / { print $2; exit }' "${SCRIPT_TMP}")
    if [ -n "${SCRIPT_SEED_URL:-}" ]; then
        SEED_URL="${SCRIPT_SEED_URL}"
    fi
else
    log "WARNING: per-MAC script not found, trying default.ipxe"
    if timeout 10 tftp "${SEED_IP}" -m binary -c get default.ipxe "${SCRIPT_TMP}" >/dev/null 2>&1; then
        SCRIPT_SEED_URL=$(awk '/^chain / { print $2; exit }' "${SCRIPT_TMP}")
        if [ -n "${SCRIPT_SEED_URL:-}" ]; then
            SEED_URL="${SCRIPT_SEED_URL}"
        fi
    fi
fi
rm -f "${SCRIPT_TMP}"

# ---------------------------------------------------------------------------
# 6. Download and install seed binaries
# ---------------------------------------------------------------------------
log "Waiting for seed HTTP server at ${SEED_URL}..."
for i in $(seq 1 30); do
    if curl -sf --max-time 3 -o /dev/null "${SEED_URL}"; then
        break
    fi
    log "  attempt ${i}/30 — retrying..."
    sleep 4
done

log "Downloading seed.tar.gz..."
SEED_TMP=$(mktemp -d)
curl -fS --retry 5 --retry-delay 2 \
    -o "${SEED_TMP}/seed.tar.gz" "${SEED_URL}"

log "Extracting binaries to /usr/local/bin/..."
tar -xzf "${SEED_TMP}/seed.tar.gz" -C /usr/local/bin/
chmod 755 /usr/local/bin/sc-*
rm -rf "${SEED_TMP}"

log "Installed: $(ls /usr/local/bin/sc-* | tr '\n' ' ')"

# ---------------------------------------------------------------------------
# 7. Write configs
# ---------------------------------------------------------------------------
log "Writing hivebus config..."
cat > /etc/hivebus/hivebus.toml << EOF
cluster_addr = "${CLUSTER_IP}"
cluster_iface = "${CLUSTER_IFACE}"
node_id = ${NODE_ID}
hostname = "${HOSTNAME}"
heartbeat_interval_ms = 150
suspect_misses = 3
dead_secs = 30
EOF

# Minimal configs for other daemons (not started automatically on agents).
cat > /etc/hivebus/imager.toml << EOF
store_dir = "/var/lib/hivebus/images"
chunk_port = 7779
EOF

cat > /etc/hivebus/netop.toml << EOF
reconcile_secs = 5
EOF

cat > /etc/hivebus/netgate.toml << EOF
dns_addr = "${SEED_IP}:5353"
domain = "cluster.internal"
EOF

cat > /etc/hivebus/orchestrator.toml << EOF
containerd_socket = "/run/containerd/containerd.sock"
cgroup_parent = "/hivebus"
image_dir = "/var/lib/hivebus/images"
EOF

# ---------------------------------------------------------------------------
# 8. Systemd unit + start hivebus
# ---------------------------------------------------------------------------
log "Installing hivebus systemd unit..."
cat > /etc/systemd/system/hivebus-hivebus.service << 'UNIT'
[Unit]
Description=hivebus hivebus
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/sc-hivebus /etc/hivebus/hivebus.toml
Restart=on-failure
RestartSec=3
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now hivebus-hivebus
systemctl is-active --quiet hivebus-hivebus

log "Agent ${HOSTNAME} (${CLUSTER_IP}, node_id=${NODE_ID}) joined cluster"
log "Node1 hivebus should now log: new hardware discovered  mac=${MAC_RAW}  node_id=${NODE_ID}"
