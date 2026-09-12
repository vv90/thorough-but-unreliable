{ modulesPath, ... }:

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
  system.stateVersion = "26.05";
}
