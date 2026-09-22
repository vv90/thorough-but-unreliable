#!/usr/bin/env bash
set -euo pipefail

# Convenience wrapper; README's real inference probe section is authoritative.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/.." && pwd)"
cd -- "$repository_root"

# Evaluation requires --impure for the local flake. Execution stays sandboxed.
# A fresh ID prevents reuse of earlier model responses as cached build outputs.
run_id="$(date +%s%N)-$$"
nix build --option sandbox true --builders '' --impure --keep-failed -L \
  --file ./nix/inference-probe.nix \
  --argstr runId "$run_id" \
  --argstr model "${INFERENCE_MODEL:-qwen3.5:9b-q4_K_M}" \
  --argstr socketPath "${INFERENCE_SOCKET:-/run/harness-inference/gateway.sock}" \
  --out-link result-inference-probe

cat result-inference-probe/artifacts/inference-probe/result.txt
