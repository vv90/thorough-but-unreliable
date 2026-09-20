{ pkgs, runtimeTests, ... }:
{
  imports = [ ./experiment-service.nix ];
  image.baseName = "experiment-smoke";
  environment.etc."experiment-smoke".text = "disposable-fixture\n";
  systemd.services.experiment-smoke = {
    wantedBy = [ "multi-user.target" ];
    wants = [ "experiment-broker.service" ];
    after = [ "experiment-broker.service" ];
    path = [
      pkgs.podman
      pkgs.coreutils
      pkgs.systemd
    ];
    script = ''
      if ${runtimeTests}/bin/podman-runtime-test --ignored --exact standalone_experiment --nocapture; then
        echo 'experiment service smoke: PASS'
      else
        echo 'experiment service smoke: FAIL' >&2
        exit 1
      fi
    '';
    serviceConfig = {
      Type = "oneshot";
      TimeoutStartSec = 120;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };
  systemd.services.experiment-smoke-poweroff = {
    wantedBy = [ "multi-user.target" ];
    wants = [ "experiment-smoke.service" ];
    after = [ "experiment-smoke.service" ];
    script = ''
      if ${pkgs.systemd}/bin/systemctl is-failed --quiet experiment-smoke.service; then
        echo 'experiment service smoke: FAIL' >&2
      fi
      ${pkgs.systemd}/bin/systemctl --no-block poweroff
    '';
    serviceConfig.Type = "oneshot";
  };
}
