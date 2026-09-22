#!/usr/bin/env bash
set -euo pipefail

# Convenience wrapper; README's full local inference run is authoritative.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/.." && pwd)"
cd -- "$repository_root"

run_id="$(date +%s%N)-$$"
nix build --option sandbox true --builders '' --impure --keep-failed -L \
  --file ./nix/harness-local.nix \
  --argstr runId "$run_id" \
  --argstr model "${INFERENCE_MODEL:-qwen3.5:9b-q4_K_M}" \
  --argstr socketPath "${INFERENCE_SOCKET:-/run/harness-inference/gateway.sock}" \
  --out-link result-harness-local

echo 'Reports and VM logs: result-harness-local/artifacts/harness-connected.*/'
