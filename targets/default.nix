{ pkgs }:
{
  smoke = import ./podman/smoke.nix { inherit pkgs; };
  config-repair = import ./podman/config-repair.nix { inherit pkgs; };
}
