# Live Podman experiment: one prepared target and one broker session per start.
{
  pkgs,
  lib,
  harnessPackage,
  targetEnvironment,
  experimentCommandNetwork ? {
    address = "10.99.2.2";
    prefix = 30;
    mac = "52:54:00:99:02:02";
  },
  ...
}:
let
  directory = "/run/experiment";
  template = pkgs.writeText "experiment-broker-config.json" (
    builtins.toJSON (
      targetEnvironment.command
      // {
        version = 1;
        listen = "${experimentCommandNetwork.address}:8080";
        socket_path = "/run/podman/podman.sock";
        command_bytes = 4096;
        runtime_json_bytes = 65536;
        stdout_bytes = 65536;
        stderr_bytes = 65536;
        command_timeout_ms = 30000;
        request_bytes = 32768;
        response_bytes = 1048576;
        connections = 4;
        read_timeout_ms = 5000;
        adapter_timeout_ms = 35000;
        write_timeout_ms = 5000;
      }
    )
  );
  cleanup = pkgs.writeShellScript "experiment-target-cleanup" ''
    set -eu
    ${pkgs.podman}/bin/podman rm --force --ignore --time 0 experiment-target
    if ${pkgs.podman}/bin/podman container exists experiment-target; then
      echo 'experiment cleanup: container still exists' >&2
      exit 1
    else
      status=$?
      test "$status" = 1
    fi
    if test -s ${directory}/cgroup; then
      cgroup=$(${pkgs.coreutils}/bin/cat ${directory}/cgroup)
      for _ in $(${pkgs.coreutils}/bin/seq 1 50); do
        if ! test -e "$cgroup"; then break; fi
        ${pkgs.coreutils}/bin/sleep 0.1
      done
      test ! -e "$cgroup"
    fi
    echo 'experiment cleanup: container absent; recorded target cgroup absent'
  '';
in
{
  imports = [ ./experiment-vm.nix ];
  networking.useNetworkd = true;
  networking.nameservers = [ ];
  networking.firewall = {
    enable = true;
    interfaces.command0.allowedTCPPorts = [ 8080 ];
  };
  systemd.network.wait-online.enable = false;
  systemd.network.links."10-command0" = {
    matchConfig.MACAddress = experimentCommandNetwork.mac;
    linkConfig.Name = "command0";
  };
  systemd.network.networks."20-command0" = {
    matchConfig.Name = "command0";
    networkConfig = {
      Address = "${experimentCommandNetwork.address}/${toString experimentCommandNetwork.prefix}";
      DHCP = false;
      IPv6AcceptRA = false;
      LinkLocalAddressing = false;
      ConfigureWithoutCarrier = true;
    };
    linkConfig.RequiredForOnline = false;
  };
  boot.kernel.sysctl = {
    "net.ipv4.ip_forward" = 0;
    "net.ipv6.conf.all.forwarding" = 0;
  };
  environment.systemPackages = [ harnessPackage ];
  systemd.services.experiment-target = {
    description = "Prepare and supervise the experiment target";
    # Only the broker requires this unit. Once it exits, stop this target too.
    unitConfig.StopWhenUnneeded = true;
    requires = [ "podman.socket" ];
    after = [
      "podman.socket"
      "systemd-networkd.service"
    ];
    path = [
      pkgs.podman
      pkgs.coreutils
      pkgs.gawk
      pkgs.iproute2
      pkgs.jq
    ];
    script = ''
      # Address assignment is asynchronous; bound startup without accepting on
      # another interface or falling back to a wildcard listener.
      ready=false
      for _ in $(seq 1 30); do
        if ip -j -4 address show dev command0 | jq -e 'any(.[].addr_info[]; .local == "${experimentCommandNetwork.address}" and .prefixlen == ${toString experimentCommandNetwork.prefix})' > /dev/null; then
          ready=true
          break
        fi
        sleep 1
      done
      test "$ready" = true
      umask 077
      podman load --input ${targetEnvironment.image}
      podman run --detach --pull=never --name experiment-target \
        --cidfile ${directory}/container-id \
        ${lib.escapeShellArgs targetEnvironment.runArgs} \
        ${lib.escapeShellArg targetEnvironment.imageReference}
      container_id=$(cat ${directory}/container-id)
      pid=$(podman inspect --format '{{.State.Pid}}' "$container_id")
      relative=$(awk -F: '$1 == "0" { print $3 }' "/proc/$pid/cgroup")
      case "$relative" in
        /*libpod-"$container_id".scope) ;;
        /*libpod-"$container_id".scope/*)
          relative="''${relative%/libpod-*}/libpod-$container_id.scope" ;;
        *) echo "unexpected target cgroup: $relative" >&2; exit 1 ;;
      esac
      printf '/sys/fs/cgroup%s\n' "$relative" > ${directory}/cgroup
      ${lib.optionalString (targetEnvironment ? prepareCommand) ''
        podman exec "$container_id" ${lib.escapeShellArgs targetEnvironment.prepareCommand}
      ''}
      jq --arg id "$container_id" '. + {container_id: $id}' ${template} > ${directory}/config.json
    '';
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # Stop the container before systemd waits for any remaining helper
      # processes. Post-stop repeats the idempotent cleanup on setup failure.
      ExecStop = cleanup;
      ExecStopPost = cleanup;
      RuntimeDirectory = "experiment";
      RuntimeDirectoryMode = "0700";
      UMask = "0077";
      TimeoutStartSec = 180;
      TimeoutStopSec = 60;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
  systemd.services.experiment-broker = {
    description = "Serve one experiment command session";
    wantedBy = [ "multi-user.target" ];
    bindsTo = [ "experiment-target.service" ];
    after = [ "experiment-target.service" ];
    serviceConfig = {
      Type = "exec";
      ExecStart = "${harnessPackage}/bin/experiment-broker --config ${directory}/config.json";
      Restart = "no";
      UMask = "0077";
      TimeoutStopSec = 60;
      RuntimeMaxSec = 900;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
}
