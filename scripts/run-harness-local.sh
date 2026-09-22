#!/usr/bin/env bash
set -euo pipefail

target_name=smoke
if [[ "${1:-}" == --target ]]; then
  if (( $# < 2 )) || [[ -z "$2" ]]; then
    echo '--target requires a target name' >&2
    exit 2
  fi
  target_name="$2"
  shift 2
fi
if (( $# > 1 )) || [[ "${1:-}" == --* ]]; then
  echo 'usage: bash scripts/run-harness-local.sh [--target NAME] [MANIFEST.json]' >&2
  exit 2
fi
manifest_args=()
if (( $# == 1 )); then
  # Resolve before changing directory; the supplied file may be outside Git.
  manifest_path="$(realpath -e -- "$1")"
  if [[ ! -f "$manifest_path" ]]; then
    echo 'manifest must be a regular file' >&2
    exit 2
  fi
  if [[ -n "${INFERENCE_MODEL:-}" ]]; then
    echo 'set the model in the supplied manifest; INFERENCE_MODEL is for the smoke test' >&2
    exit 2
  fi
  manifest_args=(--argstr manifestPath "$manifest_path")
fi

# Convenience wrapper; README's full local inference run is authoritative.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/.." && pwd)"
cd -- "$repository_root"

run_id="$(date +%s%N)-$$"
nix build --option sandbox true --builders '' --impure --keep-failed -L \
  --file ./nix/harness-local.nix \
  --argstr runId "$run_id" \
  --argstr targetName "$target_name" \
  --argstr model "${INFERENCE_MODEL:-qwen3.5:9b-q4_K_M}" \
  --argstr socketPath "${INFERENCE_SOCKET:-/run/harness-inference/gateway.sock}" \
  "${manifest_args[@]}" \
  --out-link result-harness-local

echo 'Reports and VM logs: result-harness-local/artifacts/harness-connected.*/'
