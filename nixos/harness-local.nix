{ lib, pkgs, ... }:
{
  # Reuse the smoke startup unit, replacing its fixture-specific verifier.
  systemd.services.harness-run.serviceConfig.TimeoutStartSec = lib.mkForce 720;
  systemd.services.harness-connected-smoke-result.serviceConfig.TimeoutStartSec = lib.mkForce 120;
  systemd.services.harness-connected-smoke-result.script = lib.mkForce ''
    report=/var/lib/harness/report.json
    # Export one bounded JSON line directly to the serial console, avoiding
    # journal line truncation. The runner saves it before checking semantics.
    if test -f "$report" \
      && test "$(${pkgs.coreutils}/bin/stat -c %s "$report")" -le 1048576 \
      && ${pkgs.jq}/bin/jq -c . "$report" | ${pkgs.gnused}/bin/sed 's/^/HARNESS_REPORT:/' > /dev/ttyS0; then
      :
    else
      echo 'harness local run: report export failed' >&2
    fi
    if ${pkgs.systemd}/bin/systemctl is-active --quiet harness-run.service \
      && test "$(${pkgs.coreutils}/bin/stat -c '%u:%g:%a' "$report")" = 900:900:600; then
      echo 'harness local run: COMPLETE' > /dev/ttyS0
    else
      echo 'harness local run: FAIL' > /dev/ttyS0
    fi
    ${pkgs.systemd}/bin/systemctl --no-block poweroff
  '';
}
