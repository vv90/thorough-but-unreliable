# A Podman target definition: image contents and container runtime policy.
# VM setup owns naming, lifecycle, and the binding to the command adapter.
{ pkgs }:
let
  name = "localhost/podman-runtime-target";
  tag = "latest";
  uid = 1000;
  gid = 1000;
  user = "${toString uid}:${toString gid}";
in
{
  image = pkgs.dockerTools.buildLayeredImage {
    inherit name tag;
    contents = [
      pkgs.bash
      pkgs.coreutils
    ];
    extraCommands = ''
      mkdir -p work tmp
      chmod 1777 tmp
    '';
    config = {
      User = user;
      WorkingDir = "/work";
      Env = [ "PATH=/bin" ];
      Cmd = [
        "/bin/sleep"
        "infinity"
      ];
    };
  };
  imageReference = "${name}:${tag}";
  # Settings for each fresh command shell in this target.
  command = {
    inherit uid gid;
    shell = "/bin/bash";
    workdir = "/work";
    environment = [ ];
  };
  # Individual argv entries, escaped by the caller before shell execution.
  runArgs = [
    "--network=none"
    "--read-only"
    "--cap-drop=all"
    "--security-opt=no-new-privileges"
    "--user=${user}"
    "--pids-limit=64"
    "--memory=128m"
    "--memory-swap=128m"
    "--cpus=1"
    "--tmpfs=/work:rw,nosuid,nodev,noexec,size=1m,mode=1777"
  ];
}
