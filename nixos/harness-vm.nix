{ modulesPath, pkgs, ... }:

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
  networking.useDHCP = false;
  networking.enableIPv6 = false;
  networking.firewall.enable = true;
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

  system.stateVersion = "26.05";
}
