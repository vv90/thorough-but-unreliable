{
  runId,
  model ? "qwen3.5:9b-q4_K_M",
  socketPath ? "/run/harness-inference/gateway.sock",
}:
let
  # Use the repository's pinned inputs and tracked source, excluding artifacts.
  flake = builtins.getFlake "git+file://${toString ../.}";
in
flake.packages.x86_64-linux.harness.overrideAttrs (_: {
  pname = "inference-socket-probe";
  preferLocalBuild = true;
  allowSubstitutes = false;
  INFERENCE_PROBE_RUN_ID = runId;
  INFERENCE_MODEL = model;
  INFERENCE_SOCKET = socketPath;
  cargoTestFlags = [
    "--test"
    "inference_socket"
    "real_inference"
  ];
  checkFlags = [
    "--ignored"
    "--exact"
    "--nocapture"
  ];
  installPhase = ''
    mkdir -p "$out/artifacts"
    cp -a .artifacts/inference-probe "$out/artifacts/"
  '';
})
