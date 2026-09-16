#!/usr/bin/env bash
set -euo pipefail

# Convenience wrapper; README's connected VM smoke section is authoritative.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/.." && pwd)"
cd -- "$repository_root"

nix build --option sandbox true --keep-failed -L \
  .#checks.x86_64-linux.harness-connected-smoke \
  --out-link result-harness-connected-smoke
