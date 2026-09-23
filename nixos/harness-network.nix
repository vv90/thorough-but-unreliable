{ harnessPackage, pkgs, ... }:
{
  # Apply once after device discovery and configuration media mounting. No
  # network manager races this service or installs implicit routes/resolvers.
  systemd.services.harness-network-setup = {
    description = "Apply validated harness boot network configuration";
    wantedBy = [ "multi-user.target" ];
    wants = [
      "systemd-udev-settle.service"
      "run-harness\\x2dconfig.mount"
    ];
    after = [
      "local-fs.target"
      "systemd-udev-settle.service"
      "run-harness\\x2dconfig.mount"
    ];
    before = [ "harness-network-readiness.service" ];
    path = [
      pkgs.iproute2
      pkgs.jq
      pkgs.coreutils
    ];
    script = ''
      set -euo pipefail
      config=/run/harness-config/network.json
      if test -e /dev/disk/by-label/HARNESS_CONFIG; then
        ${pkgs.util-linux}/bin/mountpoint -q /run/harness-config
      fi
      if test -e "$config" || test -L "$config"; then
        test -f "$config"
        ${harnessPackage}/bin/harness-network-config "$config" > /run/harness-network/config.json
      else
        ${harnessPackage}/bin/harness-network-config > /run/harness-network/config.json
      fi
      inference_device=
      command_device=
      inference_mac=$(jq -r .inference.mac /run/harness-network/config.json)
      command_mac=$(jq -r .command.mac /run/harness-network/config.json)
      # Resolve both devices before changing either; reject additional NICs.
      for device in /sys/class/net/*; do
        name="''${device##*/}"
        test "$name" != lo || continue
        mac=$(cat "$device/address")
        case "$mac" in
          "$inference_mac") test -z "$inference_device"; inference_device=$name ;;
          "$command_mac") test -z "$command_device"; command_device=$name ;;
          *) echo "harness network: unexpected interface $name" >&2; exit 1 ;;
        esac
      done
      test -n "$inference_device"
      test -n "$command_device"
      ip link set dev "$inference_device" down
      ip link set dev "$command_device" down
      # Temporary names also handle reversal of the final interface names.
      ip link set dev "$inference_device" name htmpinf
      ip link set dev "$command_device" name htmpcmd
      ip link set dev htmpinf name inference0
      ip link set dev htmpcmd name command0
      for role in inference command; do
        address=$(jq -r ".$role.address" /run/harness-network/config.json)
        ip address flush dev "''${role}0"
        ip address add "$address" dev "''${role}0"
        ip link set dev "''${role}0" up
      done
    '';
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      RuntimeDirectory = "harness-network";
      RuntimeDirectoryMode = "0755";
      TimeoutStartSec = 30;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
}
