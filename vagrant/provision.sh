#!/usr/bin/env bash
# provision.sh — seed-node (node1) provisioner, runs as root.
#
# Responsibilities:
#   1. Install build deps + Rust toolchain.
#   2. Build the full workspace.
#   3. Package binaries as seed.tar.gz served via HTTP on :7780.
#   4. Write configs and systemd units.
#   5. Start pxeboot (DHCP), seed HTTP server, imager, and hivebus.
#
# Agent nodes (node2/3) use provision-agent.sh which downloads
# seed.tar.gz from http://${SEED_IP}:7780/seed.tar.gz, derives
# their NODE_ID from the eth1 MAC address, and starts hivebus.
#
# Environment variables set by Vagrantfile:
#   NODE_ID       1
#   CLUSTER_IP    10.42.0.11
#   CLUSTER_IFACE eth1
#   SEED_IP       10.42.0.11

set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

NODE_ID="${NODE_ID:-1}"
CLUSTER_IP="${CLUSTER_IP:-10.42.0.11}"
CLUSTER_IFACE="${CLUSTER_IFACE:-eth1}"
SEED_IP="${SEED_IP:-10.42.0.11}"
PROJECT="/opt/hivebus"

log() { echo "[provision] $*"; }

# ---------------------------------------------------------------------------
# 1. System packages
# ---------------------------------------------------------------------------
log "Installing system packages..."
apt-get update -qq
apt-get install -y -qq \
    build-essential \
    pkg-config \
    libssl-dev \
    curl \
    git \
    python3 \
    nftables \
    iptables \
    iproute2 \
    bridge-utils \
    qemu-kvm \
    libvirt-dev \
    virt-manager \
    lxc \
    containerd \
    socat \
    jq \
    netcat-openbsd

# Enable nftables service.
systemctl enable nftables || true

# ---------------------------------------------------------------------------
# 2. Rust toolchain (installed for the vagrant user and root)
# ---------------------------------------------------------------------------
if ! command -v cargo &>/dev/null && [ ! -f /home/vagrant/.cargo/bin/cargo ]; then
    log "Installing Rust toolchain..."
    sudo -u vagrant bash -c '
        curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | \
            sh -s -- -y --default-toolchain stable --profile minimal
    '
fi

# Make cargo available in root PATH for build steps.
export PATH="/home/vagrant/.cargo/bin:$PATH"

# ---------------------------------------------------------------------------
# 3. Runtime directories
# ---------------------------------------------------------------------------
log "Creating runtime directories..."
install -d -m 755 /etc/hivebus
install -d -m 755 /var/run/hivebus
install -d -m 755 /var/lib/hivebus/images
install -d -m 755 /var/lib/hivebus/tftp
install -d -m 755 /var/lib/hivebus/tftp/scripts
install -d -m 755 /var/lib/hivebus/lxc

# ---------------------------------------------------------------------------
# 3.5. Boot artifacts for pxeboot
# ---------------------------------------------------------------------------
log "Staging PXE/TFTP boot artifacts..."
for artifact in undionly.kpxe ipxe.efi; do
    if curl -fsSL --retry 3 --retry-delay 2 \
        -o "/var/lib/hivebus/tftp/${artifact}" \
        "https://boot.ipxe.org/${artifact}"; then
        chmod 444 "/var/lib/hivebus/tftp/${artifact}"
    else
        log "WARNING: could not download ${artifact} from boot.ipxe.org"
    fi
done

cat > /var/lib/hivebus/tftp/default.ipxe << EOF
#!ipxe
echo hivebus default bootstrap
chain http://${SEED_IP}:7780/seed.tar.gz
EOF

# ---------------------------------------------------------------------------
# 4. Per-node configs
# ---------------------------------------------------------------------------
log "Writing node configs (NODE_ID=${NODE_ID})..."

cat > /etc/hivebus/hivebus.toml << EOF
cluster_addr = "${CLUSTER_IP}"
cluster_iface = "${CLUSTER_IFACE}"
node_id = ${NODE_ID}
hostname = "node${NODE_ID}"
heartbeat_interval_ms = 150
suspect_misses = 3
dead_secs = 30
EOF

cat > /etc/hivebus/netop.toml << EOF
reconcile_secs = 5
EOF

cat > /etc/hivebus/imager.toml << EOF
store_dir = "/var/lib/hivebus/images"
chunk_port = 7779
EOF

cat > /etc/hivebus/netgate.toml << EOF
dns_addr = "${CLUSTER_IP}:5353"
domain = "cluster.internal"
EOF

cat > /etc/hivebus/orchestrator.toml << EOF
containerd_socket = "/run/containerd/containerd.sock"
cgroup_parent = "/hivebus"
image_dir = "/var/lib/hivebus/images"
EOF

# pxeboot is the sole DHCP server on the cluster intnet.  It must be running
# before agent nodes are brought up with `vagrant up node2 node3`.
cat > /etc/hivebus/pxeboot.toml << EOF
server_ip  = "${SEED_IP}"
iface      = "${CLUSTER_IFACE}"
pool_start = "10.42.0.100"
pool_end   = "10.42.0.200"
tftp_root  = "/var/lib/hivebus/tftp"
boot_file  = "undionly.kpxe"
seed_url   = "http://${SEED_IP}:7780/seed.tar.gz"
EOF

# ---------------------------------------------------------------------------
# 5. Build all crates
# ---------------------------------------------------------------------------
log "Building hivebus workspace..."
cd "${PROJECT}"
sudo -u vagrant bash -c "
    export PATH=/home/vagrant/.cargo/bin:\$PATH
    cd /opt/hivebus
    cargo build --workspace 2>&1
"

# ---------------------------------------------------------------------------
# 6. Install binaries + package seed tarball
# ---------------------------------------------------------------------------
log "Installing binaries to /usr/local/bin..."
BIN_DIR="${PROJECT}/target/debug"
for bin in hivebus netop orchestrator imager netgate pxeboot hivectl; do
    if [ -f "${BIN_DIR}/${bin}" ]; then
        install -m 755 "${BIN_DIR}/${bin}" "/usr/local/bin/sc-${bin}"
    else
        log "WARNING: ${bin} binary not found (build may have failed)"
    fi
done

# Package all sc-* binaries into seed.tar.gz so agent nodes can bootstrap
# without a Rust compiler.  The archive is served by the HTTP unit below.
log "Creating seed.tar.gz..."
SEED_BINS=()
for f in /usr/local/bin/sc-*; do [ -f "$f" ] && SEED_BINS+=("$(basename "$f")"); done
if [ ${#SEED_BINS[@]} -gt 0 ]; then
    tar -czf /var/lib/hivebus/images/seed.tar.gz \
        -C /usr/local/bin "${SEED_BINS[@]}"
    log "Seed archive ready: $(du -sh /var/lib/hivebus/images/seed.tar.gz | cut -f1)"
else
    log "WARNING: no sc-* binaries found; seed.tar.gz not created"
fi

# Minimal HTTP server so agent nodes can fetch the seed archive.
# python3 ships with ubuntu/jammy64.
cat > /etc/systemd/system/hivebus-seed-http.service << 'UNIT'
[Unit]
Description=hivebus seed HTTP server (:7780)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/var/lib/hivebus/images
ExecStart=/usr/bin/python3 -m http.server 7780 --bind 0.0.0.0
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

# ---------------------------------------------------------------------------
# 7. Systemd units
# ---------------------------------------------------------------------------
log "Installing systemd units..."

write_unit() {
    local name="$1"
    local bin="$2"
    local cfg="$3"
    cat > "/etc/systemd/system/hivebus-${name}.service" << EOF
[Unit]
Description=hivebus ${name}
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/sc-${bin} ${cfg}
Restart=on-failure
RestartSec=3
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF
}

write_unit "hivebus"      "hivebus"      "/etc/hivebus/hivebus.toml"
write_unit "netop"        "netop"        "/etc/hivebus/netop.toml"
write_unit "orchestrator" "orchestrator" "/etc/hivebus/orchestrator.toml"
write_unit "imager"       "imager"       "/etc/hivebus/imager.toml"
write_unit "netgate"      "netgate"      "/etc/hivebus/netgate.toml"
write_unit "pxeboot"      "pxeboot"      "/etc/hivebus/pxeboot.toml"

systemctl daemon-reload

# Seed node: pxeboot (DHCP) and seed-http must be up *before* agent nodes
# are started so they can receive a DHCP lease and fetch seed.tar.gz.
systemctl enable --now hivebus-seed-http
systemctl enable --now hivebus-pxeboot
systemctl enable --now hivebus-imager
systemctl enable --now hivebus-hivebus

for svc in hivebus-seed-http hivebus-pxeboot hivebus-imager hivebus-hivebus; do
    systemctl is-active --quiet "$svc"
done

curl -fsS --max-time 3 "http://${SEED_IP}:7780/seed.tar.gz" >/dev/null

log "Seed node ready — DHCP (pxeboot):    ${CLUSTER_IFACE}  pool 10.42.0.100-200"
log "Seed node ready — HTTP seed server:  http://${SEED_IP}:7780/seed.tar.gz"
log "Provisioning complete for node${NODE_ID} (${CLUSTER_IP})"
