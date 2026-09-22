{
  runId,
  model ? "qwen3.5:9b-q4_K_M",
  socketPath ? "/run/harness-inference/gateway.sock",
}:
let
  flake = builtins.getFlake "git+file://${toString ../.}";
  harness = flake.nixosConfigurations.harness-paired-smoke.extendModules {
    modules = [
      ../nixos/harness-local.nix
    ];
  };
  experiment = flake.nixosConfigurations.experiment.extendModules {
    modules = [
      ({ lib, ... }: {
        systemd.services.experiment-broker.serviceConfig.RuntimeMaxSec = lib.mkForce 1300;
      })
    ];
  };
in
flake.checks.x86_64-linux.paired-vm.overrideAttrs (_: {
  pname = "harness-local-inference";
  LOCAL_RUN_ID = runId;
  INFERENCE_MODEL = model;
  INFERENCE_SOCKET = socketPath;
  HARNESS_SMOKE_IMAGE = "${harness.config.system.build.image}/harness-paired-smoke.qcow2";
  EXPERIMENT_IMAGE = "${experiment.config.system.build.image}/experiment.qcow2";
  cargoTestFlags = [
    "--test"
    "connected_vm"
    "paired::local::local_inference"
  ];
})
