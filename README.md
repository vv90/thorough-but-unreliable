# thorough-but-unreliable

## Harness base image

`nixos/harness-vm.nix` defines the initial x86_64 NixOS guest. The flake exposes
`nixosConfigurations.harness` and `packages.x86_64-linux.harness-image`.
It produces `harness.qcow2`, with its runtime Nix store inside the disk,
legacy BIOS/GRUB boot, an 8 GiB virtual disk, and diagnostics on serial port 0
at 115200 baud. Login is locked and DHCP, IPv6 and forwarding are disabled. A
noninteractive `harness` service account owns `/var/lib/harness`; a boot-time
readiness unit verifies its fixed UID/GID and state-directory access.
The application and the two network interfaces are not configured yet.

On the external Linux builder with KVM available:

```sh
nix build .#harness-image --out-link result-harness
```

The image builder itself requires KVM. This unprivileged devcontainer can
evaluate the derivation but cannot assemble or boot it locally.
The output disk is `result-harness/harness.qcow2`.

### Run a boot smoke test with KVM

Run the following commands from the repository root on the KVM host. Create a
disposable writable overlay so the built base image remains unchanged:

```sh
mkdir -p .artifacts
qemu-img create \
  -f qcow2 \
  -F qcow2 \
  -b "$(readlink -f result-harness/harness.qcow2)" \
  .artifacts/harness-smoke.qcow2
```

Use a fresh overlay filename for each test. Start the guest with a virtio disk,
legacy BIOS, a serial console, and no network interface or host directory:

```sh
qemu-system-x86_64 \
  -enable-kvm \
  -machine q35 \
  -cpu host \
  -m 1024 \
  -drive file=.artifacts/harness-smoke.qcow2,format=qcow2,if=virtio \
  -nic none \
  -nographic \
  -no-reboot
```

A successful boot reaches the multi-user target, prints the following readiness
message, and displays `harness login:`:

```text
harness readiness: uid=900 gid=900 state-directory=writable
```

Login is intentionally locked. Exit QEMU by pressing `Ctrl-A`, then `X`. From
another terminal, `pgrep -af harness-smoke.qcow2` shows whether this test VM is
still running. The image derivation evaluates against the pinned Nixpkgs
revision; repeat this smoke test after rebuilding the image.

## Development container

Copy these files into the new `thorough-but-unreliable` repository. Merge the flake outputs
with that project's flake rather than replacing an existing application flake.
This is a configuration proposal, not a deployed container.
The Nixpkgs lock metadata is reused from the existing host repository. JSON,
and Nix syntax checks passed, and the image derivation evaluated against
the pinned revision. Image building and editor attachment still need validation.

## Applied changes

- Image reference: `localhost/thorough-but-unreliable-dev:latest`. Use the
  `latest` tag for now; rebuild, load and recreate after image changes.
- Start with private Nix, Nix editing tools, and basic editor/CLI utilities.
  Python, uv, pytest, Ruff and Pyright are deferred until the base image works.
- A dedicated Codex state volume is persisted; no tool-cache volumes are added.
  No full home, host caches, host credentials, runtime sockets or devices are mounted.
  The source checkout is deliberately writable.

## Build and load

From the new repository root, on the host or designated builder with Nix and
rootless Podman:

```sh
nix build .#devImage
podman load < result
```

Nix creates `result` as a symlink to the image archive in the Nix store. Podman
loads that archive as `localhost/thorough-but-unreliable-dev:latest`, matching the
devcontainer configuration. Then open or recreate the devcontainer in your
editor; loading an image does not update an existing container.

While building this proposal directly from its Git-ignored handoff directory,
use `nix build "path:$PWD#devImage"` instead of `nix build .#devImage`.

Repeat the build/load commands after image changes, then recreate the container.
Build/load automation and image-identity recording are deferred. Run these
commands explicitly, not from a devcontainer lifecycle hook.

The memory limits (8 GiB RAM, 10 GiB total RAM+swap) are initial examples. Adjust
them to system RAM and other workloads; GPU VRAM is unrelated. Verify rootless
cgroup limits on the actual host.

## Persistent Codex state

The named Podman volume `thorough-but-unreliable-codex` mounts at
`/home/dev/.codex`. It retains credentials, configuration and local conversation
state across container recreation. It is not a mount of the host's Codex home.
Keep this name stable to reuse the state; choose another name for an unrelated
project or an independent checkout that should not share it.

The image prepares this directory with mode 0700 and UID/GID 1000 ownership,
and a writable mode-0600 `config.toml` containing
`cli_auth_credentials_store = "file"`. Podman's initial volume copy populates
a new volume. Existing volumes retain their own files: rebuilding the image
does not replace their configuration or repair their permissions.

After building/loading the image and attaching, check and sign in:

```sh
test -w /home/dev/.codex
stat -c '%u:%g %a %n' /home/dev/.codex /home/dev/.codex/config.toml
codex login
codex login status
```

Recreate the container with the same volume and run `codex login status` again
to verify persistence. File-backed credentials are supported by the
[official Codex authentication documentation](https://learn.chatgpt.com/docs/auth).
Revoking a login requires reauthentication but does not delete this volume's
configuration or local threads. Deleting the volume does remove that state.

Credentials are created only at login, never baked into the image or Nix store.
Other programs running as `dev` can read them; directory permissions do not
isolate programs sharing that user. Do not attach this volume to runtime harness
or experiment VMs. Treat backups as sensitive and manage retention explicitly.
Volume initialization, ownership and login persistence still need runtime testing.

## Private Nix installation

Nix runs locally as `dev` (UID/GID 1000). The image enables `includeNixDB` to
register included store closures and their GC roots. Image store paths and
database directories belong to that user. `NIX_REMOTE=local` selects the private
store rather than a daemon. Flakes and `nix-command` are enabled.

Both `/nix/store` and `/nix/var/nix` live in the container writable layer. They
survive stopping/restarting that instance but are reset on recreation. No host
store, database, Nix daemon socket or additional Nix volume is mounted. Do not
mount an empty volume over only one half of this initialized store/database.

No additional binary cache, substituter, signing key or cache service is
configured. Nix retains its default public cache configuration. HTTPS CA paths
are supplied for Nix and SSL-aware clients.

Interactive `nix eval`, `nix develop`, package builds and non-VM checks can run
inside the container. The configuration explicitly sets `sandbox = false` and
an empty build-users group for this restricted single-user development setup.
These commands remain inside the Podman boundary but do not receive a second
Nix build sandbox. Authoritative sandboxed builds and KVM integration remain on
the external runner. Do not add container privileges to make those work here.

After building and attaching, verify:

```sh
nix --version
nix config show sandbox
nix config show substituters
test -w /nix/store
test -w /nix/var/nix/db
nix eval --raw .#packages.x86_64-linux.devImage.name
nix develop --command bash --version
```

Then perform a small package build and confirm the resulting path is registered
in the private store. These are acceptance checks, not results already obtained.

## Explicit remaining decisions

- Verify HTTPS with Nix, curl and the agent; some clients use their
  own certificate-discovery rules. Do not disable TLS verification.
- Validate editor server requirements with this Nix userspace.
  Host-distro library paths may not exist. This package
  list is a baseline, not a claim that Zed/VS Code attachment was tested.
- Codex is retained from the supplied example and pinned through Nixpkgs.
  Only its dedicated state directory is persisted, not all of `/home/dev`.
- Networking is the runtime default. It does not block host/LAN access. A
  dedicated host-managed network and policy are separate deployment work.
- Editor credential/SSH-agent forwarding must be disabled or explicitly scoped
  in editor settings and verified in the resulting container.
- Authoritative package checks and VM integration run externally against a
  recorded source snapshot, without exposing host Nix/libvirt/Podman sockets.
- Add language tools and scoped cache volumes incrementally after validating
  the base image. Decide retention/quotas when adding persistent caches.

No host configuration or running development environment is changed by saving
this proposal. Validate it with the chosen editor and rootless Podman before use.
