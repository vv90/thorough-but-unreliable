{
  harnessPackage,
  lib,
  pkgs,
  ...
}:

let
  expectedReport = ../tests/fixtures/connected-report.json;
in
{
  # Opt-in test image only. The base image never launches a trial at boot.
  image.baseName = lib.mkForce "harness-connected-smoke";

  systemd.services.harness-run = {
    description = "Run the connected harness smoke trial";
    wantedBy = [ "multi-user.target" ];
    requires = [
      "harness-readiness.service"
      "harness-network-readiness.service"
      "harness-config-check.service"
      "run-harness\\x2dconfig.mount"
    ];
    after = [
      "harness-readiness.service"
      "harness-network-readiness.service"
      "harness-config-check.service"
      "run-harness\\x2dconfig.mount"
    ];
    script = ''
      # Never overwrite an earlier trial's report on restart or reused disks.
      set -C
      exec ${harnessPackage}/bin/harness run \
        --manifest /run/harness-config/manifest.json \
        > /var/lib/harness/report.json
    '';
    serviceConfig = {
      Type = "oneshot";
      User = "harness";
      Group = "harness";
      StateDirectory = "harness";
      StateDirectoryMode = "0700";
      UMask = "0077";
      Restart = "no";
      RemainAfterExit = true;
      TimeoutStartSec = 60;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };

  # Wants rather than Requires: also run the verifier when the trial failed.
  systemd.services.harness-connected-smoke-result = {
    description = "Check the connected smoke report and shut down";
    wantedBy = [ "multi-user.target" ];
    wants = [ "harness-run.service" ];
    after = [ "harness-run.service" ];
    script = ''
      report=/var/lib/harness/report.json
      if ${pkgs.systemd}/bin/systemctl is-active --quiet harness-run.service \
        && test "$(${pkgs.coreutils}/bin/stat -c '%u:%g:%a' "$report")" = 900:900:600 \
        && ${pkgs.jq}/bin/jq -e -s --slurpfile expected ${expectedReport} \
          'length == 1 and .[0] == $expected[0]' "$report" > /dev/null; then
        echo 'harness connected smoke: PASS'
      else
        echo 'harness connected smoke: FAIL' >&2
      fi
      ${pkgs.systemd}/bin/systemctl --no-block poweroff
    '';
    serviceConfig = {
      Type = "oneshot";
      TimeoutStartSec = 15;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
}
