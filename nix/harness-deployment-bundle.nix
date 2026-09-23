{
  pkgs,
  harnessImage,
  harnessPackage,
}:
pkgs.runCommand "harness-deployment-bundle"
  {
    nativeBuildInputs = [
      harnessPackage
      pkgs.qemu-utils
      pkgs.jq
      pkgs.gnutar
      pkgs.gzip
    ];
  }
  ''
    set -euo pipefail
    mkdir -p harness-deployment/examples "$out"
    cp --sparse=always ${harnessImage}/harness-run.qcow2 harness-deployment/harness-run.qcow2
    cp ${../trials/hello.json} harness-deployment/examples/manifest.json
    cp ${../deployment/network.json} harness-deployment/examples/network.json
    cp ${../DEPLOYMENT.md} harness-deployment/DEPLOYMENT.md
    cp ${../README.md} harness-deployment/README.md
    cp ${../COMMAND_PROTOCOL.md} harness-deployment/COMMAND_PROTOCOL.md

    # Validate shipped examples against the same executables as the image.
    harness check-config --manifest harness-deployment/examples/manifest.json
    harness-network-config harness-deployment/examples/network.json > /dev/null
    qemu-img info --output=json harness-deployment/harness-run.qcow2 > disk-info.json
    jq -e '.format == "qcow2" and (has("backing-filename") | not)' disk-info.json > /dev/null
    jq -n --arg sourceImage '${harnessImage}/harness-run.qcow2' \
      '{bundle_version: 1, architecture: "x86_64", firmware: "legacy-bios", source_image: $sourceImage}' \
      > harness-deployment/build-info.json

    (
      cd harness-deployment
      sha256sum harness-run.qcow2 examples/manifest.json examples/network.json \
        DEPLOYMENT.md README.md COMMAND_PROTOCOL.md build-info.json > SHA256SUMS
    )
    # Regular files only, normalized metadata, no Nix-store symlink dependencies.
    chmod -R u=rwX,go=rX harness-deployment
    chmod a-w harness-deployment/harness-run.qcow2
    tar --sort=name --mtime=@1 --owner=0 --group=0 --numeric-owner \
      -cf - harness-deployment | gzip -n > "$out/harness-deployment.tar.gz"
    (
      cd "$out"
      sha256sum harness-deployment.tar.gz > SHA256SUMS
    )
  ''
