{
  harnessPackage,
  lib,
  pkgs,
  ...
}:
{
  image.baseName = "harness-run";

  systemd.services.harness-run = {
    description = "Run the supplied harness trial";
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
      # A fresh disposable disk is required; never overwrite an earlier report.
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
      # Exit 1 can represent a recorded non-submission outcome. The result
      # service also requires a valid report before declaring completion.
      SuccessExitStatus = [ 1 ];
      TimeoutStartSec = lib.mkDefault 720;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };

  systemd.services.harness-result = {
    description = "Export the harness report and shut down";
    wantedBy = [ "multi-user.target" ];
    wants = [ "harness-run.service" ];
    after = [ "harness-run.service" ];
    script = ''
      set -o pipefail
      report=/var/lib/harness/report.json
      exported=false
      # Require exactly one report object, not an empty file or JSON stream.
      if test -f "$report" \
        && test "$(${pkgs.coreutils}/bin/stat -c %s "$report")" -le 1048576 \
        && ${pkgs.jq}/bin/jq -e -s 'length == 1 and (.[0] | type == "object")' "$report" > /dev/null \
        && ${pkgs.jq}/bin/jq -c . "$report" | ${pkgs.gnused}/bin/sed 's/^/HARNESS_REPORT:/' > /dev/ttyS0; then
        exported=true
      else
        echo 'harness run: report export failed' >&2
      fi
      if test "$exported" = true \
        && ${pkgs.systemd}/bin/systemctl is-active --quiet harness-run.service \
        && test "$(${pkgs.coreutils}/bin/stat -c '%u:%g:%a' "$report")" = 900:900:600; then
        echo 'harness run: COMPLETE' > /dev/ttyS0
      else
        echo 'harness run: FAIL' > /dev/ttyS0
      fi
      ${pkgs.systemd}/bin/systemctl --no-block poweroff
    '';
    serviceConfig = {
      Type = "oneshot";
      TimeoutStartSec = 120;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
}
