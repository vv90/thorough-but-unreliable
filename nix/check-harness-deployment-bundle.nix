{ pkgs, harnessPackage }:
let
  # This tests packaging without booting or claiming to test a real VM image.
  image =
    pkgs.runCommand "deployment-bundle-fixture-image"
      {
        nativeBuildInputs = [ pkgs.qemu-utils ];
      }
      ''
        mkdir -p "$out"
        qemu-img create -f qcow2 "$out/harness-run.qcow2" 1M
      '';
  bundle = import ./harness-deployment-bundle.nix {
    inherit pkgs harnessPackage;
    harnessImage = image;
  };
in
pkgs.runCommand "check-harness-deployment-bundle"
  {
    nativeBuildInputs = [
      pkgs.gnutar
      pkgs.gzip
      pkgs.findutils
    ];
  }
  ''
    set -euo pipefail
    (cd ${bundle}; sha256sum --check SHA256SUMS)
    tar -xzf ${bundle}/harness-deployment.tar.gz
    cd harness-deployment
    sha256sum --check SHA256SUMS
    test -z "$(find . -type l -print)"
    cmp harness-run.qcow2 ${image}/harness-run.qcow2
    cmp examples/manifest.json ${../trials/hello.json}
    cmp examples/network.json ${../deployment/network.json}
    touch "$out"
  ''
