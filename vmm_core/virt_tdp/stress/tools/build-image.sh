#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail
usage() {
    echo "Usage: $0 BASE_UBUNTU_QCOW2 OUTPUT_VHDX [DOCKER_STATIC_TGZ]" >&2
    exit 2
}
(( $# == 2 || $# == 3 )) || usage
base=$(realpath "$1")
output=$(realpath -m "$2")
docker_tgz=${3:-}
[[ -f $base ]] || { echo "base image not found: $base" >&2; exit 1; }
if [[ -z $docker_tgz ]]; then
    docker_tgz=$(dirname "$output")/docker-28.2.2.tgz
    [[ -f $docker_tgz ]] || curl -fL -o "$docker_tgz" \
        https://download.docker.com/linux/static/stable/x86_64/docker-28.2.2.tgz
fi
docker_tgz=$(realpath "$docker_tgz")

here=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/virt-tdp-image.XXXXXX")
loop= mountpoint=
cleanup() {
    [[ -z $mountpoint ]] || sudo umount "$mountpoint" 2>/dev/null || true
    [[ -z $loop ]] || sudo losetup -d "$loop" 2>/dev/null || true
}
trap cleanup EXIT

cc -O2 -static -pthread "$here/guest/workload-probe.c" -o "$tmp/workload-probe"
cat >"$tmp/Dockerfile" <<'EOF'
FROM scratch
COPY workload-probe /workload-probe
ENTRYPOINT ["/workload-probe"]
EOF
sudo su kvin -c "docker pull postgres:15-alpine"
sudo su kvin -c "docker pull redis:7-alpine"
sudo su kvin -c "docker build -t virt-tdp-workload:latest '$tmp'"
sudo su kvin -c "docker save postgres:15-alpine redis:7-alpine virt-tdp-workload:latest -o '$tmp/images.tar'"
gzip -n -1 "$tmp/images.tar"

cp --reflink=auto "$base" "$tmp/ubuntu.qcow2"
qemu-img resize "$tmp/ubuntu.qcow2" 12G
qemu-img convert -O raw -S 4k "$tmp/ubuntu.qcow2" "$tmp/ubuntu.raw"
loop=$(sudo losetup --find --show --partscan "$tmp/ubuntu.raw")
sudo growpart "$loop" 1
sudo e2fsck -f -y "${loop}p1" >/dev/null
sudo resize2fs "${loop}p1" >/dev/null
mountpoint="$tmp/root"
mkdir "$mountpoint"
sudo mount "${loop}p1" "$mountpoint"

docker_dir="$tmp/docker"
mkdir "$docker_dir"
tar -xzf "$docker_tgz" -C "$docker_dir" --strip-components=1
sudo install -m 0755 "$docker_dir"/* "$mountpoint/usr/local/bin/"
sudo install -d "$mountpoint/opt/virt-tdp-stress" "$mountpoint/etc/docker" \
    "$mountpoint/etc/systemd/system/multi-user.target.wants"
sudo install -m 0644 "$tmp/images.tar.gz" "$mountpoint/opt/virt-tdp-stress/images.tar.gz"
sudo install -m 0755 "$here/guest/run-workloads.sh" "$mountpoint/opt/virt-tdp-stress/run-workloads.sh"
sudo install -m 0644 "$here/guest/virt-tdp-stress.service" "$mountpoint/etc/systemd/system/virt-tdp-stress.service"
sudo ln -sf /etc/systemd/system/virt-tdp-stress.service \
    "$mountpoint/etc/systemd/system/multi-user.target.wants/virt-tdp-stress.service"
sudo ln -sf /etc/systemd/system/docker.service \
    "$mountpoint/etc/systemd/system/multi-user.target.wants/docker.service"

sudo tee "$mountpoint/etc/docker/daemon.json" >/dev/null <<'EOF'
{"bridge":"none","data-root":"/var/lib/docker","exec-opts":["native.cgroupdriver=systemd"],"iptables":false,"ip6tables":false,"ip-forward":false,"storage-driver":"vfs"}
EOF
sudo tee "$mountpoint/etc/systemd/system/docker.service" >/dev/null <<'EOF'
[Unit]
Description=Docker Application Container Engine
After=network.target local-fs.target
[Service]
Type=notify
ExecStart=/usr/local/bin/dockerd --config-file=/etc/docker/daemon.json
Restart=on-failure
RestartSec=2
Delegate=yes
KillMode=process
TasksMax=infinity
[Install]
WantedBy=multi-user.target
EOF
for unit in cloud-init-local.service cloud-init.service cloud-config.service cloud-final.service \
    apt-daily.service apt-daily-upgrade.service apt-daily.timer apt-daily-upgrade.timer; do
    sudo ln -sf /dev/null "$mountpoint/etc/systemd/system/$unit"
done
echo virt-tdp-ubuntu | sudo tee "$mountpoint/etc/hostname" >/dev/null
sudo truncate -s 0 "$mountpoint/etc/machine-id"
sudo sync
sudo umount "$mountpoint"; mountpoint=
sudo losetup -d "$loop"; loop=

mkdir -p "$(dirname "$output")"
qemu-img convert -O vhdx -o subformat=dynamic,block_size=2097152 "$tmp/ubuntu.raw" "$output"
qemu-img check "$output"
qemu-img info "$output"
