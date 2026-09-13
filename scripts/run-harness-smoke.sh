#!/usr/bin/env bash

set -euo pipefail

# Convenience wrapper for the Harness base image instructions in README.md.
# Keep the README as the source of truth and update it first when this workflow
# changes.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/.." && pwd)"
cd -- "$repository_root"

nix build .#harness-image --out-link result-harness

mkdir -p .artifacts
run_directory="$(mktemp -d .artifacts/harness-smoke.XXXXXX)"
config_directory="$run_directory/config"
config_iso="$run_directory/harness-config.iso"
overlay="$run_directory/harness.qcow2"

mkdir -p "$config_directory"
printf '%s\n' '{"version":1,"run_id":"smoke"}' \
  > "$config_directory/manifest.json"
nix shell nixpkgs#xorriso --command xorrisofs \
  -quiet \
  -volid HARNESS_CONFIG \
  -joliet \
  -rock \
  -output "$config_iso" \
  "$config_directory"

qemu-img create \
  -f qcow2 \
  -F qcow2 \
  -b "$(readlink -f result-harness/harness.qcow2)" \
  "$overlay"

echo "harness smoke-test artifacts: $run_directory"
exec qemu-system-x86_64 \
  -enable-kvm \
  -machine q35 \
  -cpu host \
  -m 1024 \
  -drive "file=$overlay,format=qcow2,if=virtio" \
  -drive "file=$config_iso,format=raw,media=cdrom,readonly=on" \
  -nic none \
  -device virtio-net-pci,mac=52:54:00:99:01:02 \
  -device virtio-net-pci,mac=52:54:00:99:02:01 \
  -nographic \
  -no-reboot
