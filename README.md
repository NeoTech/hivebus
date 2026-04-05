# hivebus

A Rust-native micro-clustering platform. Six lightweight daemons coordinate
node discovery, workload orchestration, networking, image distribution, DNS,
and PXE boot over a private cluster network.

---

## Architecture

```
 ┌──────────────────────────────────────┐
 │  node1 (seed)       10.42.0.11       │  builds workspace
 │  pxeboot  ← DHCP :67               │  sole DHCP server on intnet
 │  seed-http← HTTP :7780             │  serves seed.tar.gz (built binaries)
 │  hivebus  ← UDP :7777              │  master / membership
 │  imager   ← TCP :7779              │  binary chunk store
 └───────────────┬──────────────────────┘
                 │  hivebus-net (10.42.0.0/24, VirtualBox intnet)
                 │  no external DHCP — pxeboot is sole server
       ┌─────────┴──────────┐
       ▼                    ▼
 ┌──────────────┐    ┌──────────────┐
 │ node2        │    │ node3        │  receive DHCP lease from pxeboot
 │ IP: dynamic  │    │ IP: dynamic  │  NODE_ID derived from eth1 MAC
 │ hivebus      │    │ hivebus      │  announces on UDP :7777
 └──────────────┘    └──────────────┘
```

### Daemons

| Crate | Binary | Port / Socket | Purpose |
|---|---|---|---|
| `proto` | _lib_ | — | Shared wire types; no protobuf; bincode v2 |
| `hivebus` | `hv-hivebus` | UDP 7777, `/var/run/hivebus/hivebus.sock` | Heartbeat, failure detection, gossip, leader election |
| `orchestrator` | `hv-orchestrator` | `/var/run/hivebus/orchestrator.sock` | KVM / LXC / containerd workload CRUD |
| `netop` | `hv-netop` | `/var/run/hivebus/netop.sock` | nftables rules + virtual interface reconciliation |
| `imager` | `hv-imager` | TCP 7779, `/var/run/hivebus/imager.sock` | BLAKE3-hashed P2P image chunk store |
| `netgate` | `hv-netgate` | UDP/TCP 5353, `/var/run/hivebus/netgate.sock` | Internal DNS + L4 TCP proxy |
| `pxeboot` | `hv-pxeboot` | UDP 67/68 + TFTP 69, `/var/run/hivebus/pxeboot.sock` | DHCP + TFTP + generated per-MAC iPXE scripts |
| `hivectl` | `hv-hivectl` | _(reads hivebus socket)_ | TUI cluster inspector — run on any node |

All daemons share a common control-plane framing over Unix sockets:
```
op(1B) | len(2B BE) | payload(bincode ControlOp)
```

Discovery frames on the cluster network:
```
magic(0xCA,1B) | type_seq(1B: hi4=MsgType, lo4=seq) | node_id(2B BE) | payload(bincode)
```

---

## Repository layout

```
hivebus/
├── Cargo.toml              ← workspace root (resolver = "2", shared deps)
├── Vagrantfile             ← seed + agent PXE-boot topology
├── vagrant/
│   ├── provision.sh        ← seed node: build workspace, seed.tar.gz, HTTP + TFTP boot assets
│   ├── provision-agent.sh  ← agent nodes: DHCP lease, MAC→NODE_ID, TFTP script, download seed
│   └── test-cluster.sh     ← integration test runner (runs from host)
└── crates/
    ├── proto/              ← FrameHeader, MsgType, NodeInfo, ControlOp, ControlReply
    ├── hivebus/
    ├── orchestrator/
    ├── netop/
    ├── imager/
    ├── netgate/
    ├── pxeboot/
    └── hivectl/            ← TUI cluster inspector (hv-hivectl)
```

---

## Prerequisites

### Host (for Vagrant workflow)

| Tool | Version |
|---|---|
| [VirtualBox](https://www.virtualbox.org/) | 7.x |
| [Vagrant](https://www.vagrantup.com/) | 2.4+ |

### For native Linux builds

```bash
# Ubuntu / Debian
sudo apt-get install build-essential pkg-config libssl-dev \
    nftables iproute2 qemu-kvm libvirt-dev lxc containerd

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

---

## Vagrant workflow

The cluster uses a **seed + agent PXE-boot model** that mirrors how the
platform works on real hardware:

```
 node1 (seed)                           node2 / node3 (agents)
 ──────────────────────────────────     ──────────────────────────────────
 builds Rust workspace                  no compiler needed
 packages binaries → seed.tar.gz        gets DHCP lease from pxeboot
 serves seed.tar.gz on HTTP :7780       fetches per-MAC iPXE script via TFTP
 runs pxeboot (DHCP :67, TFTP :69)     downloads seed.tar.gz from script URL
 runs hivebus + imager                  starts hivebus → announces to bus
                                        node1 logs "new hardware discovered"
```

The cluster intnet (`hivebus-net`, `10.42.0.0/24`) has no host-side DHCP
server — `pxeboot` on node1 is the sole DHCP authority, exactly as it would
be on bare-metal.

In this Vagrant environment, the generated iPXE script is consumed by
`provision-agent.sh` after the guest OS boots, which lets the repo exercise
DHCP + TFTP + per-MAC bootstrap metadata even though Vagrant is still booting
from its box disk rather than firmware PXE.

### Step 1 — bring up the seed node

```bash
# ~10 min first run: downloads box, installs Rust, builds workspace,
# packages seed.tar.gz, starts pxeboot + hivebus + imager.
vagrant up node1
```

Verify the seed is ready before starting agents:

```bash
vagrant ssh node1 -- "systemctl is-active hivebus-pxeboot hivebus-seed-http hivebus-hivebus"
vagrant ssh node1 -- "curl -sI http://10.42.0.11:7780/seed.tar.gz | head -2"
vagrant ssh node2 -- "mac=\$(cat /sys/class/net/eth1/address); tmp=\$(mktemp); tftp 10.42.0.11 -m binary -c get scripts/\${mac}.ipxe \"\$tmp\" && cat \"\$tmp\""
```

### Step 2 — boot agent nodes

```bash
vagrant up node2 node3
```

`provision-agent.sh` runs on each agent and:

1. Waits for a DHCP lease on `eth1` from node1's `pxeboot` daemon
2. Reads the `eth1` MAC address
3. Derives `NODE_ID` with the same XOR fold as `proto::node_id_from_mac`
4. Pulls `scripts/<mac>.ipxe` from node1 over TFTP and extracts the seed archive URL
5. Downloads `seed.tar.gz` and extracts `hv-*` binaries
6. Writes `/etc/hivebus/hivebus.toml` with the MAC-derived ID and DHCP IP
7. Starts `hivebus-hivebus` — broadcasts an ANNOUNCE on UDP 7777

Watch node1 log new members joining:

```bash
vagrant ssh node1 -- "journalctl -u hivebus-hivebus --no-pager -n 30"
# look for:
#   new hardware discovered via DHCP  mac=…  node_id=…
#   node announced  node_id=…  ip=…
```

### Rebuilding after code changes

Source is synced to `/opt/hivebus` on node1. Rebuild and refresh the seed
archive so agents pick up new binaries on next provision:

```bash
vagrant ssh node1 -- "
    cd /opt/hivebus && cargo build --workspace &&
    tar -czf /var/lib/hivebus/images/seed.tar.gz \
        -C /usr/local/bin \$(ls /usr/local/bin/hv-*)
"
# re-provision agents to pull the new archive
vagrant provision node2 node3
```

Or re-run the full seed provisioner:

```bash
vagrant provision node1
```

### Start / stop daemons

```bash
vagrant ssh node1

sudo systemctl status  hivebus-hivebus
sudo journalctl -fu    hivebus-hivebus
sudo journalctl -fu    hivebus-pxeboot

# Inspect generated TFTP assets
ls -R /var/lib/hivebus/tftp

# Optional daemons (not auto-started)
sudo systemctl start hivebus-netop
sudo systemctl start hivebus-orchestrator
sudo systemctl start hivebus-netgate
```

### Tear down

```bash
vagrant halt          # suspend VMs (keep disk state)
vagrant destroy -f    # delete VMs completely
```

### `hv-hivectl` — TUI cluster inspector

Run on any node that has a hivebus socket.  No arguments needed on-node; pass
the socket path when connecting remotely via forwarded socket.

```bash
vagrant ssh node1

# Connect to local hivebus socket
hv-hivectl

# Explicit socket path
hv-hivectl /var/run/hivebus/hivebus.sock
```

```
╔═ hivebus hivectl ══════════════════════════════════════════════════════╗
║ ✔  socket: /var/run/hivebus/hivebus.sock   master: node1 (1)           ║
║    nodes: 3/3 alive   updated 0s ago                                       ║
╠═ nodes ═══════════════════════════════════════════════════════════════════╣
║ NODE ID  HOSTNAME         ADDRESS                STATE      ★   LAST SEEN ║
║                                                                            ║
║ 1        node1            10.42.0.11:7778        Alive      ★   0s        ║
║ 2        node2            10.42.0.105:7778       Alive          1s        ║
║ 3        node3            10.42.0.106:7778       Suspect        9s        ║
╚═══════════════════════════════════════════════════════════════════════════╝
  [q] quit   [r] refresh   [↑ ↓] select   [ctrl-c] quit
```

Keys: `q` quit · `r` force refresh · `↑`/`↓` select row · auto-refreshes every 1 s

---

## Integration tests

Run the integration suite from the **host** after `vagrant up`:

```bash
bash vagrant/test-cluster.sh
```

Tests performed:

| Test | What it checks |
|---|---|
| `binaries_installed` | All `hv-*` binaries present on each node |
| `cargo_check` | Workspace compiles without errors |
| `proto_unit_tests` | `cargo test -p proto` passes |
| `cluster_network_reachability` | node1 can ping agent nodes on `10.42.0.0/24` |
| `seed_http_smoke` | Agent can reach `seed.tar.gz` over HTTP |
| `pxeboot_running_smoke` | `hivebus-pxeboot` systemd unit is active on node1 |
| `pxeboot_default_script_smoke` | Agent can fetch `default.ipxe` over TFTP |
| `pxeboot_mac_script_smoke` | Agent can fetch its generated `scripts/<mac>.ipxe` over TFTP |
| `hivebus_running` | `hivebus-hivebus` systemd unit active on all 3 nodes |
| `hivebus_socket` | Control socket `/var/run/hivebus/hivebus.sock` exists |
| `hivebus_control_socket_smoke` | socat can connect to the control socket |
| `hivebus_discovery_3nodes` | node1 log shows announce events from peer nodes |
| `orchestrator_socket_smoke` | Orchestrator control socket present after manual start |
| `netop_socket_smoke` | Netop control socket present after manual start |
| `imager_socket_smoke` | Imager control socket present after manual start |
| `netgate_socket_smoke` | Netgate control socket present after manual start |
| `netgate_stub_honesty` | Confirms DNS remains a documented stub |
| `netop_stub_honesty` | Confirms reconciliation remains a documented stub |

The smoke suite is intentionally honest: `netgate` DNS and `netop` reconciliation are still stubs, so the tests verify startup and explicit stub status rather than pretending the features work end to end.

---

## Native development (Linux only)

Many daemons use Linux-only syscall crates (`nix`, `kvm-ioctls`, `nftables`,
`rtnetlink`).  Building and running natively requires a Linux host with the
packages listed under Prerequisites.

```bash
# Check everything compiles
cargo check --workspace

# Run proto unit tests (no Linux syscalls required — works on any OS)
cargo test -p proto

# Build in release mode
cargo build --workspace --release

# Run hivebus on the cluster interface
RUST_LOG=info sudo ./target/release/hv-hivebus \
    --config /etc/hivebus/hivebus.toml
```

### Config files

Each daemon reads a TOML config from `/etc/hivebus/<name>.toml`.
Provision.sh writes these automatically in Vagrant.  For native dev, create
them manually:

**`/etc/hivebus/hivebus.toml`**
```toml
cluster_addr   = "10.42.0.11"
cluster_iface  = "eth1"
node_id        = 1          # optional; derived from MAC if omitted
hostname       = "node1"
heartbeat_interval_ms = 150
suspect_misses = 3
dead_secs      = 30
```

**`/etc/hivebus/pxeboot.toml`**
```toml
server_ip   = "10.42.0.11"
iface       = "eth1"
pool_start  = "10.42.0.100"
pool_end    = "10.42.0.200"
tftp_root   = "/var/lib/hivebus/tftp"
boot_file   = "undionly.kpxe"
seed_url    = "http://10.42.0.11:7780/seed.tar.gz"
```

**`/etc/hivebus/imager.toml`**
```toml
store_dir  = "/var/lib/hivebus/images"
chunk_port = 7779
```

**`/etc/hivebus/netgate.toml`**
```toml
dns_addr = "10.42.0.11:5353"
domain   = "cluster.internal"
```

---

## Wire protocol quick reference

### Discovery frame (UDP broadcast + multicast `224.0.133.7:7777`)

```
 0       1       2       3       4      ...
 ┌───────┬───────┬───────────────┬──────────────────┐
 │ 0xCA  │type   │   node_id     │ bincode payload  │
 │ magic │+seq   │   (2B BE)     │                  │
 └───────┴───────┴───────────────┴──────────────────┘
          ↑ hi4 = MsgType  lo4 = rolling seq 0–15
```

`MsgType`: `Heartbeat=0` · `Announce=1` · `Leave=2` · `Ack=3` · `Gossip=4` · `VoteRequest=5` · `VoteGrant=6`

### Control plane (Unix socket)

```
 0       1       2       3       4      ...
 ┌───────┬───────────────┬──────────────────────────┐
 │  op   │  payload len  │ bincode ControlOp/Reply  │
 │ (1B)  │   (2B BE)     │                          │
 └───────┴───────────────┴──────────────────────────┘
```

`ControlOp`: `Ping` · `GetNodes` · `GetMaster` · `StepDown` · `Shutdown` · `Custom{tag, payload}`

---

## Licence

MIT
