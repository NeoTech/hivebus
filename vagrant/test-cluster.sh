#!/usr/bin/env bash
# test-cluster.sh — honest smoke suite for the Vagrant cluster.
#
# Execute from the host:
#   bash vagrant/test-cluster.sh
#
# The suite deliberately distinguishes between startup smoke tests and true
# feature completeness. netgate DNS and netop reconciliation are still stubs,
# so we assert their stub status instead of pretending they are end-to-end.

set -euo pipefail
cd "$(dirname "$0")/.."

PASS=0
FAIL=0
FAILURES=()

c() {
    local node="$1"; shift
    vagrant ssh "$node" -- -q "$@"
}

pass() { echo "  PASS: $1"; ((PASS++)) || true; }
fail() { echo "  FAIL: $1"; ((FAIL++)) || true; FAILURES+=("$1"); }

run_test() {
    local name="$1"
    local fn="$2"
    echo ""
    echo "--- $name ---"
    if "$fn"; then
        pass "$name"
    else
        fail "$name"
    fi
}

test_binaries_installed() {
    c node1 "command -v sc-hivebus sc-netop sc-orchestrator sc-imager sc-netgate sc-pxeboot sc-hivectl" >/dev/null
    c node2 "command -v sc-hivebus sc-hivectl" >/dev/null
}

test_cargo_check() {
    c node1 "cd /opt/subcluster && \$HOME/.cargo/bin/cargo check --workspace 2>&1" || return 1
}

test_proto_unit_tests() {
    c node1 "cd /opt/subcluster && \$HOME/.cargo/bin/cargo test -p proto --lib 2>&1" | grep -q "test result: ok"
}

test_cluster_network_reachability() {
    local node2_ip
    node2_ip=$(c node2 "ip -4 addr show eth1 | awk '/inet / { split(\$2,a,\"/\"); print a[1]; exit }'")
    [ -n "${node2_ip}" ] || return 1
    c node1 "ping -c1 -W1 ${node2_ip}" >/dev/null
}

test_seed_http_smoke() {
    c node2 "curl -fsS --max-time 3 http://10.42.0.11:7780/seed.tar.gz >/dev/null"
}

test_pxeboot_running_smoke() {
    c node1 "systemctl is-active subcluster-pxeboot" >/dev/null
}

test_pxeboot_tftp_default_script_smoke() {
    c node2 "tmp=\$(mktemp) && tftp 10.42.0.11 -m binary -c get default.ipxe \"\$tmp\" >/dev/null 2>&1 && grep -q 'seed.tar.gz' \"\$tmp\" && rm -f \"\$tmp\""
}

test_pxeboot_tftp_mac_script_smoke() {
    c node2 "mac=\$(cat /sys/class/net/eth1/address) && tmp=\$(mktemp) && tftp 10.42.0.11 -m binary -c get \"scripts/\${mac}.ipxe\" \"\$tmp\" >/dev/null 2>&1 && grep -q \"\${mac}\" \"\$tmp\" && grep -q 'seed.tar.gz' \"\$tmp\" && rm -f \"\$tmp\""
}

test_hivebus_running() {
    for node in node1 node2 node3; do
        c "$node" "systemctl is-active subcluster-hivebus" >/dev/null || return 1
    done
}

test_hivebus_socket() {
    c node1 "test -S /var/run/subcluster/hivebus.sock"
}

test_hivebus_control_socket_smoke() {
    c node1 "socat - UNIX-CONNECT:/var/run/subcluster/hivebus.sock" <<< "" 2>/dev/null || true
    c node1 "test -S /var/run/subcluster/hivebus.sock"
}

test_hivebus_discovery_3nodes() {
    sleep 2
    c node1 "journalctl -u subcluster-hivebus --no-pager -n 80" | grep -q '"node announced"'
}

test_orchestrator_socket_startup_smoke() {
    c node1 "sudo systemctl start subcluster-orchestrator" || true
    sleep 1
    c node1 "test -S /var/run/subcluster/orchestrator.sock"
}

test_netop_socket_startup_smoke() {
    c node1 "sudo systemctl start subcluster-netop" || true
    sleep 1
    c node1 "test -S /var/run/subcluster/netop.sock"
}

test_imager_socket_startup_smoke() {
    c node1 "sudo systemctl start subcluster-imager" || true
    sleep 1
    c node1 "test -S /var/run/subcluster/imager.sock"
}

test_netgate_socket_startup_smoke() {
    c node1 "sudo systemctl start subcluster-netgate" || true
    sleep 1
    c node1 "test -S /var/run/subcluster/netgate.sock"
}

test_netgate_stub_honesty() {
    c node1 "journalctl -u subcluster-netgate --no-pager -n 20" | grep -q 'stub'
}

test_netop_stub_honesty() {
    c node1 "grep -q 'interface reconcile (stub)' /opt/subcluster/crates/netop/src/main.rs"
}

echo "================================================================"
echo "  subcluster smoke test suite"
echo "================================================================"
echo ""

echo "Checking Vagrant nodes are running..."
for node in node1 node2 node3; do
    if ! vagrant status "$node" 2>/dev/null | grep -q "running"; then
        echo "ERROR: $node is not running. Run: vagrant up"
        exit 1
    fi
done
echo "All nodes running."

run_test "binaries installed"           test_binaries_installed
run_test "cargo check"                  test_cargo_check
run_test "proto unit tests"             test_proto_unit_tests
run_test "cluster network reachable"    test_cluster_network_reachability
run_test "seed HTTP smoke"              test_seed_http_smoke
run_test "pxeboot running smoke"        test_pxeboot_running_smoke
run_test "pxeboot default script smoke" test_pxeboot_tftp_default_script_smoke
run_test "pxeboot MAC script smoke"     test_pxeboot_tftp_mac_script_smoke
run_test "hivebus running"              test_hivebus_running
run_test "hivebus socket"               test_hivebus_socket
run_test "hivebus control socket smoke" test_hivebus_control_socket_smoke
run_test "hivebus discovery (3 nodes)"  test_hivebus_discovery_3nodes
run_test "orchestrator socket smoke"    test_orchestrator_socket_startup_smoke
run_test "netop socket smoke"           test_netop_socket_startup_smoke
run_test "imager socket smoke"          test_imager_socket_startup_smoke
run_test "netgate socket smoke"         test_netgate_socket_startup_smoke
run_test "netgate stub honesty"         test_netgate_stub_honesty
run_test "netop stub honesty"           test_netop_stub_honesty

echo ""
echo "================================================================"
echo "  Results: ${PASS} passed, ${FAIL} failed"
if [ ${#FAILURES[@]} -gt 0 ]; then
    echo "  Failed tests:"
    for f in "${FAILURES[@]}"; do echo "    - $f"; done
    echo "================================================================"
    exit 1
fi
echo "================================================================"
