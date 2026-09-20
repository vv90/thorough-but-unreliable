{
  pkgs,
  modulesPath,
  runtimeTests,
  ...
}:
let
  targetImage = pkgs.dockerTools.buildLayeredImage {
    name = "localhost/podman-runtime-target";
    tag = "latest";
    contents = [
      pkgs.bash
      pkgs.coreutils
    ];
    extraCommands = ''
      mkdir -p work tmp
      chmod 1777 tmp
    '';
    config = {
      User = "1000:1000";
      WorkingDir = "/work";
      Env = [ "PATH=/bin" ];
      Cmd = [
        "/bin/sleep"
        "infinity"
      ];
    };
  };
in
{
  imports = [
    (modulesPath + "/profiles/minimal.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    (modulesPath + "/virtualisation/disk-image.nix")
  ];
  image = {
    baseName = "podman-runtime-smoke";
    format = "qcow2";
    efiSupport = false;
  };
  virtualisation.diskSize = 8192;
  networking.hostName = "podman-runtime-smoke";
  networking.useDHCP = false;
  networking.enableIPv6 = false;
  services.resolved.enable = false;
  services.openssh.enable = false;
  users.mutableUsers = false;
  users.allowNoPasswordLogin = true;
  users.users.root.hashedPassword = "!";
  boot.kernelParams = [ "console=ttyS0,115200n8" ];
  boot.loader.grub.extraConfig = ''
    serial --unit=0 --speed=115200
    terminal_input serial
    terminal_output serial
  '';
  services.journald.settings.Journal = {
    ForwardToConsole = true;
    TTYPath = "/dev/ttyS0";
  };
  virtualisation.podman.enable = true;
  systemd.sockets.podman.socketConfig.SocketMode = "0600";
  environment.etc."podman-runtime-smoke".text = "disposable-fixture\n";

  # Rootful Podman belongs only to this disposable VM. The test has control of
  # its socket; the non-root target has no socket or VM directory mounts.
  systemd.services.podman-runtime-smoke = {
    wantedBy = [ "multi-user.target" ];
    requires = [ "podman.socket" ];
    after = [ "podman.socket" ];
    path = [
      pkgs.podman
      pkgs.coreutils
      pkgs.gawk
      pkgs.gnugrep
    ];
    script = ''
      # Last-resort cleanup also runs on assertion/setup failure. Success is
      # reported only after Rust supervision and this independent check agree.
      finish() {
        status=$?
        trap - EXIT
        if ! podman rm --force --ignore --time 0 runtime-target; then
          status=1
        fi
        if ! remaining=$(podman ps -aq --no-trunc); then
          status=1
        elif test -n "$remaining"; then
          status=1
        fi
        if test -s /run/podman-runtime-smoke/cgroup; then
          cgroup=$(cat /run/podman-runtime-smoke/cgroup)
          if test -e "$cgroup"; then status=1; fi
        fi
        if test "$status" = 0; then
          echo 'podman runtime smoke: PASS'
        else
          echo 'podman runtime smoke: FAIL' >&2
        fi
        exit "$status"
      }
      trap finish EXIT
      podman --version
      podman load --input ${targetImage}
      podman run --detach --pull=never --name runtime-target \
        --network=none --read-only --cap-drop=all \
        --security-opt=no-new-privileges --user=1000:1000 \
        --pids-limit=64 --memory=128m --memory-swap=128m --cpus=1 \
        --tmpfs /work:rw,nosuid,nodev,noexec,size=1m,mode=1777 \
        localhost/podman-runtime-target:latest \
        > /run/podman-runtime-smoke/container-id
      container_id=$(cat /run/podman-runtime-smoke/container-id)
      pid=$(podman inspect --format '{{.State.Pid}}' "$container_id")
      # Observe the target cgroup from the VM's procfs, not from target output.
      relative=$(awk -F: '$1 == "0" { print $3 }' "/proc/$pid/cgroup")
      case "$relative" in
        /*libpod-"$container_id".scope) ;;
        /*libpod-"$container_id".scope/*)
          relative="''${relative%/libpod-*}/libpod-$container_id.scope"
          ;;
        *) echo "unexpected target cgroup: $relative" >&2; exit 1 ;;
      esac
      printf '/sys/fs/cgroup%s\n' "$relative" > /run/podman-runtime-smoke/cgroup
      ${runtimeTests}/bin/podman-runtime-test --ignored --exact real_podman --nocapture
    '';
    serviceConfig = {
      Type = "oneshot";
      RuntimeDirectory = "podman-runtime-smoke";
      RuntimeDirectoryMode = "0700";
      TimeoutStartSec = 180;
      TimeoutStopSec = 30;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
  # Run even when setup or assertions fail; the outer Rust runner also imposes
  # a hard deadline and kills/reaps a VM that never reaches shutdown.
  systemd.services.podman-runtime-poweroff = {
    wantedBy = [ "multi-user.target" ];
    wants = [ "podman-runtime-smoke.service" ];
    after = [ "podman-runtime-smoke.service" ];
    script = ''
      if ! ${pkgs.systemd}/bin/systemctl is-failed --quiet podman-runtime-smoke.service; then
        echo 'podman runtime guest: finished'
      else
        echo 'podman runtime smoke: FAIL' >&2
      fi
      ${pkgs.systemd}/bin/systemctl --no-block poweroff
    '';
    serviceConfig.Type = "oneshot";
  };
  system.stateVersion = "26.05";
}
