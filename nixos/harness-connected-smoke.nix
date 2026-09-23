{
  lib,
  pkgs,
  connectedExpectedReport,
  connectedImageName,
  ...
}:

let
  expectedReport = connectedExpectedReport;
in
{
  imports = [ ./harness-run.nix ];
  image.baseName = lib.mkForce connectedImageName;

  systemd.services.harness-run.serviceConfig.TimeoutStartSec = 60;
  systemd.services.harness-result = {
    wants = [ "harness-connected-smoke-result.service" ];
    after = [ "harness-connected-smoke-result.service" ];
  };

  # Wants rather than Requires: also run the verifier when the trial failed.
  systemd.services.harness-connected-smoke-result = {
    description = "Check the connected smoke report";
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
    '';
    serviceConfig = {
      Type = "oneshot";
      TimeoutStartSec = 15;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
}
