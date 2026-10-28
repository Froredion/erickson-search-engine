#!/usr/bin/env bash
# Runs on the instance (up.sh calls it): stripes the local NVMe disks into one volume at /data,
# installs the AWS CLI and Rust, and builds the engine. Safe to run again.
set -euo pipefail

if ! mountpoint -q /data; then
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq mdadm build-essential pkg-config unzip curl >/dev/null
  # Instance-store NVMe drives, not the EBS root volume.
  mapfile -t DISKS < <(lsblk -dno NAME,MODEL | awk '/Instance Storage/ {print "/dev/" $1}')
  if [ "${#DISKS[@]}" -eq 0 ]; then
    echo "no instance-store disks found; using the root volume" >&2
    sudo mkdir -p /data
  else
    DEV=${DISKS[0]}
    if [ "${#DISKS[@]}" -gt 1 ]; then
      sudo mdadm --create /dev/md0 --level=0 --raid-devices="${#DISKS[@]}" "${DISKS[@]}" --run
      DEV=/dev/md0
    fi
    sudo mkfs.ext4 -q -F -E nodiscard,lazy_itable_init=1,lazy_journal_init=1 "$DEV"
    sudo mkdir -p /data
    sudo mount -o noatime "$DEV" /data
  fi
  sudo chown "$USER" /data
fi
df -h /data | tail -1

if ! command -v aws >/dev/null; then
  curl -fsSL https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip -o /tmp/awscli.zip
  unzip -q -o /tmp/awscli.zip -d /tmp
  sudo /tmp/aws/install >/dev/null
fi

if ! command -v cargo >/dev/null && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
cd "$HOME/erickson"
cargo build --release -q
echo "built $HOME/erickson/target/release/erickson"
