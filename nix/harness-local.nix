{
  runId,
  model ? "qwen3.5:9b-q4_K_M",
  socketPath ? "/run/harness-inference/gateway.sock",
  manifestPath ? null,
  targetName ? "smoke",
}:
let
  flake = builtins.getFlake "git+file://${toString ../.}";
  pkgs = flake.nixosConfigurations.experiment.pkgs;
  targets = import ../targets { inherit pkgs; };
  target =
    targets.${targetName}
      or (throw "unknown target '${targetName}'; available: ${builtins.concatStringsSep ", " (builtins.attrNames targets)}");
  targetRecord = pkgs.writeText "target.json" (
    builtins.toJSON {
      name = targetName;
      image = toString target.image;
      inherit (target) imageReference command runArgs;
      prepareCommand = target.prepareCommand or [ ];
    }
  );
  harness = flake.nixosConfigurations.harness-paired-smoke.extendModules {
    modules = [
      ../nixos/harness-local.nix
      (
        { lib, ... }:
        lib.mkIf (manifestPath != null) {
          # The CLI returns 1 for a recorded non-submission outcome. A missing
          # or invalid report still fails the runner's collection checks.
          systemd.services.harness-run.serviceConfig.SuccessExitStatus = [ 1 ];
        }
      )
    ];
  };
  experiment = flake.nixosConfigurations.experiment.extendModules {
    specialArgs.targetEnvironment = target;
    modules = [
      ({ lib, ... }: {
        systemd.services.experiment-broker.serviceConfig.RuntimeMaxSec = lib.mkForce 1300;
      })
    ];
  };
in
if targetName != "smoke" && manifestPath == null then
  throw "a non-smoke target requires a supplied trial manifest"
else
  flake.checks.x86_64-linux.paired-vm.overrideAttrs (old: {
    pname = "harness-local-inference";
    LOCAL_RUN_ID = runId;
    INFERENCE_MODEL = model;
    INFERENCE_SOCKET = socketPath;
    TRIAL_MANIFEST =
      if manifestPath == null then
        ""
      else
        builtins.path {
          path = builtins.toPath manifestPath;
          name = "trial-manifest.json";
        };
    HARNESS_SMOKE_IMAGE = "${harness.config.system.build.image}/harness-paired-smoke.qcow2";
    EXPERIMENT_IMAGE = "${experiment.config.system.build.image}/experiment.qcow2";
    cargoTestFlags = [
      "--test"
      "connected_vm"
      "paired::local::local_inference"
    ];
    installPhase = old.installPhase + ''
      cp ${targetRecord} "$out/artifacts/target.json"
    '';
  })
