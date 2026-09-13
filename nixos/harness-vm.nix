{
  harnessPackage,
  modulesPath,
  pkgs,
  ...
}:

{
  imports = [
    (modulesPath + "/profiles/minimal.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    (modulesPath + "/virtualisation/disk-image.nix")
  ];

  image = {
    baseName = "harness";
    format = "qcow2";
    efiSupport = false;
  };
  virtualisation.diskSize = 8192;

  networking.hostName = "harness";
  networking.useNetworkd = true;
  networking.useDHCP = false;
  networking.enableIPv6 = false;
  networking.nameservers = [ ];
  networking.firewall.enable = true;
  services.resolved.enable = false;
  systemd.network.wait-online.enable = false;
  systemd.network.links = {
    "10-inference0" = {
      matchConfig.MACAddress = "52:54:00:99:01:02";
      linkConfig.Name = "inference0";
    };
    "10-command0" = {
      matchConfig.MACAddress = "52:54:00:99:02:01";
      linkConfig.Name = "command0";
    };
  };
  systemd.network.networks = {
    "20-inference0" = {
      matchConfig.Name = "inference0";
      networkConfig = {
        Address = "10.99.1.2/24";
        DHCP = false;
        IPv6AcceptRA = false;
        LinkLocalAddressing = false;
        ConfigureWithoutCarrier = true;
      };
      linkConfig.RequiredForOnline = false;
    };
    "20-command0" = {
      matchConfig.Name = "command0";
      networkConfig = {
        Address = "10.99.2.1/30";
        DHCP = false;
        IPv6AcceptRA = false;
        LinkLocalAddressing = false;
        ConfigureWithoutCarrier = true;
      };
      linkConfig.RequiredForOnline = false;
    };
  };
  boot.kernel.sysctl = {
    "net.ipv4.ip_forward" = 0;
    "net.ipv6.conf.all.forwarding" = 0;
  };

  boot.kernelParams = [ "console=ttyS0,115200n8" ];
  boot.loader.grub = {
    configurationLimit = 1;
    extraConfig = ''
      serial --unit=0 --speed=115200
      terminal_input serial
      terminal_output serial
    '';
  };
  services.journald.settings.Journal = {
    ForwardToConsole = true;
    TTYPath = "/dev/ttyS0";
  };

  # This base image exposes boot diagnostics; interactive login stays locked.
  services.openssh.enable = false;
  users.mutableUsers = false;
  # Acknowledge intentional lockout; this does not unlock the root password.
  users.allowNoPasswordLogin = true;
  users.users.root.hashedPassword = "!";

  users.groups.harness.gid = 900;
  users.users.harness = {
    isSystemUser = true;
    uid = 900;
    group = "harness";
    home = "/var/lib/harness";
    hashedPassword = "!";
    shell = pkgs.shadow;
  };

  environment.systemPackages = [ harnessPackage ];

  systemd.services.harness-readiness = {
    description = "Verify the harness service account";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" ];
    script = ''
      test "$(id -u)" = 900
      test "$(id -g)" = 900
      test -w /var/lib/harness
      echo "harness readiness: uid=900 gid=900 state-directory=writable"
    '';
    serviceConfig = {
      Type = "oneshot";
      User = "harness";
      Group = "harness";
      StateDirectory = "harness";
      StateDirectoryMode = "0700";
      RemainAfterExit = true;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };

  systemd.services.harness-network-readiness = {
    description = "Verify the harness network interfaces";
    wantedBy = [ "multi-user.target" ];
    wants = [ "systemd-networkd.service" ];
    after = [ "systemd-networkd.service" ];
    script = ''
      address_present() {
        ${pkgs.iproute2}/bin/ip -4 -o address show dev "$1" \
          | ${pkgs.gnugrep}/bin/grep -Fq " inet $2 "
      }

      addresses_ready=false
      for _ in $(${pkgs.coreutils}/bin/seq 1 50); do
        if address_present inference0 10.99.1.2/24 \
          && address_present command0 10.99.2.1/30; then
          addresses_ready=true
          break
        fi
        ${pkgs.coreutils}/bin/sleep 0.1
      done

      if test "$addresses_ready" != true; then
        echo "harness network: expected static addresses were not configured" >&2
        ${pkgs.iproute2}/bin/ip -4 -o address show >&2
        exit 1
      fi

      inference_mac="$(cat /sys/class/net/inference0/address)"
      command_mac="$(cat /sys/class/net/command0/address)"
      if test "$inference_mac" != "52:54:00:99:01:02"; then
        echo "harness network: unexpected inference0 MAC $inference_mac" >&2
        exit 1
      fi
      if test "$command_mac" != "52:54:00:99:02:01"; then
        echo "harness network: unexpected command0 MAC $command_mac" >&2
        exit 1
      fi

      for interface_path in /sys/class/net/*; do
        interface_name="''${interface_path##*/}"
        case "$interface_name" in
          lo|inference0|command0) ;;
          *)
            echo "harness network: unexpected interface $interface_name" >&2
            exit 1
            ;;
        esac
      done

      if ${pkgs.iproute2}/bin/ip -4 route show default \
        | ${pkgs.gnugrep}/bin/grep -q .; then
        echo "harness network: unexpected IPv4 default route" >&2
        exit 1
      fi
      if ${pkgs.iproute2}/bin/ip -6 route show default \
        | ${pkgs.gnugrep}/bin/grep -q .; then
        echo "harness network: unexpected IPv6 default route" >&2
        exit 1
      fi
      if ${pkgs.gnugrep}/bin/grep -Eq '^[[:space:]]*nameserver[[:space:]]' /etc/resolv.conf; then
        echo "harness network: unexpected DNS resolver" >&2
        exit 1
      fi

      echo "harness network: inference0=10.99.1.2/24 command0=10.99.2.1/30 default-route=none dns=none"
    '';
    serviceConfig = {
      Type = "oneshot";
      User = "harness";
      Group = "harness";
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };

  systemd.mounts = [
    {
      description = "Harness per-run configuration";
      what = "/dev/disk/by-label/HARNESS_CONFIG";
      where = "/run/harness-config";
      type = "iso9660";
      options = "ro,nosuid,nodev,noexec";
      wantedBy = [ "multi-user.target" ];
      before = [ "harness-config-check.service" ];
      unitConfig.ConditionPathExists = "/dev/disk/by-label/HARNESS_CONFIG";
    }
  ];

  systemd.services.harness-config-check = {
    description = "Check the harness per-run configuration";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" ];
    script = ''
      if ! test -e /dev/disk/by-label/HARNESS_CONFIG; then
        echo "harness config: no configuration media attached"
        exit 0
      fi

      manifest=/run/harness-config/manifest.json
      if ! test -r "$manifest" || ! test -s "$manifest"; then
        echo "harness config: manifest.json missing, unreadable, or empty" >&2
        exit 1
      fi

      ${harnessPackage}/bin/harness check-config --manifest "$manifest"
    '';
    serviceConfig = {
      Type = "oneshot";
      User = "harness";
      Group = "harness";
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };

  system.stateVersion = "26.05";
}
