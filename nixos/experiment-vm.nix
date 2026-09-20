# Shared experiment VM foundation. Target lifecycle and the command service
# are supplied by the consuming configuration.
{
  lib,
  modulesPath,
  ...
}:
{
  imports = [
    (modulesPath + "/profiles/minimal.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    (modulesPath + "/virtualisation/disk-image.nix")
  ];
  image = {
    baseName = lib.mkDefault "experiment";
    format = "qcow2";
    efiSupport = false;
  };
  virtualisation.diskSize = 8192;
  networking.hostName = lib.mkDefault "experiment";
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
  # The current experiment backend uses Podman inside this disposable VM.
  virtualisation.podman.enable = true;
  systemd.sockets.podman.socketConfig.SocketMode = "0600";
  system.stateVersion = "26.05";
}
