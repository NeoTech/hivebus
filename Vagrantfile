# -*- mode: ruby -*-
# vi: set ft=ruby :
#
# Subcluster 3-node topology — seed + agent PXE boot model.
#
# node1 (seed)
#   - Static IP 10.42.0.11 on the cluster intnet.
#   - Builds the entire Rust workspace.
#   - Packages binaries as /var/lib/subcluster/images/seed.tar.gz.
#   - Serves seed.tar.gz via HTTP on port 7780.
#   - Runs pxeboot (DHCP server, UDP :67) + hivebus + imager.
#
# node2/3 (agents)
#   - No static IP — receive address from node1's pxeboot DHCP server.
#   - provision-agent.sh downloads seed.tar.gz, derives NODE_ID from
#     the eth1 MAC address (same XOR fold as proto::node_id_from_mac),
#     writes /etc/subcluster/hivebus.toml, and starts hivebus.
#   - hivebus announces itself; node1 logs "new hardware discovered".
#
# Boot order:
#   vagrant up node1            # provision seed first (~10 min)
#   vagrant up node2 node3      # agents bootstrap from node1

SEED_IP = "10.42.0.11"
INTNET  = "subcluster-net"

Vagrant.configure("2") do |config|
  config.vm.box = "ubuntu/jammy64"

  # Disable default /vagrant mount; we mount the project root explicitly.
  config.vm.synced_folder ".", "/vagrant", disabled: true
  config.vm.synced_folder ".", "/opt/subcluster", type: "virtualbox"

  # ── node1: seed node ─────────────────────────────────────────────────────
  config.vm.define "node1", primary: true do |vm|
    vm.vm.hostname = "node1"

    # Static cluster IP — this node is the pxeboot DHCP server.
    vm.vm.network "private_network",
      ip: SEED_IP,
      virtualbox__intnet: INTNET

    vm.vm.provider "virtualbox" do |vb|
      vb.name   = "subcluster-node1"
      vb.memory = 2048
      vb.cpus   = 2
      vb.customize ["modifyvm", :id, "--nested-hw-virt", "on"]
      vb.customize ["modifyvm", :id, "--audio", "none"]
      vb.customize ["modifyvm", :id, "--usb", "off"]
    end

    vm.vm.provision "shell",
      path: "vagrant/provision.sh",
      env: {
        "NODE_ID"       => "1",
        "CLUSTER_IP"    => SEED_IP,
        "CLUSTER_IFACE" => "eth1",
        "SEED_IP"       => SEED_IP,
      }
  end

  # ── node2/3: agent nodes ─────────────────────────────────────────────────
  # These VMs have no static IP.  They receive a DHCP lease from node1's
  # pxeboot daemon and self-configure from their MAC address.
  [2, 3].each do |n|
    config.vm.define "node#{n}" do |vm|
      vm.vm.hostname = "node#{n}"

      # DHCP on the cluster intnet — pxeboot on node1 is the sole DHCP server.
      # No VirtualBox built-in DHCP server exists on intnet networks.
      vm.vm.network "private_network",
        type: "dhcp",
        virtualbox__intnet: INTNET

      vm.vm.provider "virtualbox" do |vb|
        vb.name   = "subcluster-node#{n}"
        vb.memory = 2048
        vb.cpus   = 2
        vb.customize ["modifyvm", :id, "--nested-hw-virt", "on"]
        vb.customize ["modifyvm", :id, "--audio", "none"]
        vb.customize ["modifyvm", :id, "--usb", "off"]
        # e1000 NIC on eth1 for PXE ROM support (future true-PXE path).
        vb.customize ["modifyvm", :id, "--nictype2", "82540EM"]
      end

      vm.vm.provision "shell",
        path: "vagrant/provision-agent.sh",
        env: {
          "SEED_IP"       => SEED_IP,
          "CLUSTER_IFACE" => "eth1",
        }
    end
  end
end
