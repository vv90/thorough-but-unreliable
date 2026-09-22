{ pkgs }:
let
  base = import ./smoke.nix { inherit pkgs; };
  name = "localhost/config-repair-target";
  app = pkgs.writeShellApplication {
    name = "orders-summary";
    runtimeInputs = [ pkgs.jq ];
    text = builtins.readFile ./config-repair/orders-summary.sh;
  };
  check = pkgs.writeShellApplication {
    name = "check-orders";
    runtimeInputs = [
      app
      pkgs.jq
    ];
    text = ''
      if (( $# > 1 )); then
        echo 'usage: check-orders [CONFIG.json]' >&2
        exit 2
      fi
      result="$(orders-summary "''${1:-/work/config.json}")"
      if jq -e '. == {orders: 3, total: 42}' <<< "$result" > /dev/null; then
        echo 'PASS: 3 orders, total 42'
      else
        printf 'FAIL: unexpected summary: %s\n' "$result" >&2
        exit 1
      fi
    '';
  };
  seed = pkgs.runCommand "config-repair-workspace" { } ''
    mkdir -p "$out/opt/task"
    cp -R ${./config-repair/workspace}/. "$out/opt/task/"
  '';
  prepare = pkgs.writeShellApplication {
    name = "prepare-workspace";
    runtimeInputs = [ pkgs.coreutils ];
    text = ''
      if (( $# > 1 )); then
        echo 'usage: prepare-workspace [DIRECTORY]' >&2
        exit 2
      fi
      cp -R --no-preserve=mode ${seed}/opt/task/. "''${1:-/work}/"
    '';
  };
in
base
// {
  imageReference = "${name}:latest";
  # Executed synchronously as the container's configured user before broker
  # startup. The readonly image seed is copied into the existing bounded tmpfs.
  prepareCommand = [ "/bin/prepare-workspace" ];
  image = pkgs.dockerTools.buildLayeredImage {
    inherit name;
    tag = "latest";
    contents = [
      pkgs.bash
      pkgs.coreutils
      pkgs.findutils
      pkgs.gnugrep
      pkgs.gnused
      pkgs.jq
      app
      check
      prepare
      seed
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
  # A deterministic build check of the exercise, independent of model behavior.
  check =
    pkgs.runCommand "config-repair-check"
      {
        nativeBuildInputs = [
          app
          check
          prepare
          pkgs.jq
        ];
      }
      ''
        mkdir workspace
        prepare-workspace "$PWD/workspace"
        test -w workspace/config.json
        if check-orders workspace/config.json > before.json 2> before.log; then
          echo 'broken configuration unexpectedly passed' >&2
          exit 1
        fi
        grep -F 'input file not found: /work/data/orders.json' before.log
        jq --arg path "$PWD/workspace/fixtures/orders.json" '.input = $path' \
          workspace/config.json > fixed.json
        orders-summary fixed.json > after.json
        jq -e '. == {orders: 3, total: 42}' after.json
        check-orders fixed.json
        mkdir -p "$out"
        cp before.log after.json "$out/"
      '';
}
