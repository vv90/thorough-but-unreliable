# Deploying the standalone harness

The deployment bundle contains a runnable x86_64 NixOS QCOW2 image, editable
configuration examples, this guide, the command protocol, a snapshot of the
repository README, build metadata, and SHA-256 checksums. The image contains its
runtime Nix store: the destination does not need NixOS, Nix, Rust, Python, or a
repository checkout to boot it. An x86_64 Linux KVM host with QEMU is the tested
platform. Other hypervisors and software emulation are not verified here.

The repository [README](README.md) is authoritative for build/run commands and
limits; its source-file links require a checkout. This guide describes the
deployment contract. The bundle does not include an experiment VM, target,
inference server, gateway, or launcher. Supply those separately.

## 1. Build and transfer

From the repository on the external KVM builder:

```sh
nix build --option sandbox true --builders '' -L \
  .#harness-deployment-bundle --out-link result-harness-deployment
```

Transfer the **files** `result-harness-deployment/harness-deployment.tar.gz` and
`result-harness-deployment/SHA256SUMS` to the destination. Copying just the
`result-harness-deployment` symlink will not transfer the bundle.

In their destination directory:

```sh
sha256sum --check SHA256SUMS
tar -xzf harness-deployment.tar.gz
cd harness-deployment
sha256sum --check SHA256SUMS
```

Checksums detect transfer damage; they are not signatures. `build-info.json`
records the source image store path as provenance, not as a runtime dependency.
Keep the extracted image and examples unchanged; create per-run copies below.

## 2. Provide the two services and isolated networks

For the unmodified examples, arrange:

| Role | Harness MAC | Harness address | Service the harness contacts |
| --- | --- | --- | --- |
| Inference | `52:54:00:99:01:02` | `10.99.1.2/24` | `http://10.99.1.1:11434/v1/chat/completions` |
| Command | `52:54:00:99:02:01` | `10.99.2.1/30` | `http://10.99.2.2:8080/v1/command` |

Inference must support OpenAI-compatible chat completions with tool calls and
the configured model. The broker must implement [command protocol v1](COMMAND_PROTOCOL.md)
with a fresh session and a prepared disposable target. The harness does not
prepare or clean up that target. Start the services and establish readiness
before booting the harness.

The launcher connects each NIC to its intended isolated network. Direct isolated
virtual networks or narrowly scoped forwarding can provide the endpoints. The
host Unix inference socket used by this repository is one forwarding mechanism;
the image itself uses HTTP and has no dependency on that socket or its host path.
A local gateway can also forward to a cloud inference provider and hold its
credentials outside the harness. The harness currently has no API-key setting.

There must be exactly two non-loopback NICs. The guest has no default route,
DNS, DHCP, IPv6, or forwarding. Use reachable on-subnet IP endpoints. For another
layout, edit both examples and configure matching NICs and peers; see the README
for `network.json` validation rules. Omitting `network.json` selects the defaults
above. Invalid configuration blocks trial startup; it does not select defaults.

## 3. Prepare fresh per-run media

Perform runtime preparation inside the deployment's isolated runner environment.
The following uses QEMU tools and `xorrisofs`; it does not start a VM. From the
extracted bundle directory, choose a fresh run directory:

```sh
mkdir run-001
mkdir run-001/config
cp examples/manifest.json examples/network.json run-001/config/
```

Edit `run-001/config/manifest.json`: choose a unique `run_id`, task, model, endpoint
URLs, and budgets. Edit `network.json` if changing the layout. The included
manifest is the hello trial; its model name is an example, not a bundled model.
Keep credentials out of these files. Then create the ISO and writable overlay:

```sh
xorrisofs -quiet -volid HARNESS_CONFIG -joliet -rock \
  -output run-001/harness-config.iso run-001/config
qemu-img create -f qcow2 -F qcow2 \
  -b "$(pwd)/harness-run.qcow2" run-001/disk.qcow2
```

Keep the base image at that path for the run: the overlay references it. The
runner must expose that same path inside its filesystem namespace. Never reuse
a completed overlay or write directly to the base. A full disposable copy of
the base also works if the hypervisor cannot use backing files. Transfer the
base bundle, not an overlay with a dependency on another machine's filesystem.

## 4. Boot through an isolated launcher

Configure the deployment's VM manager with:

| Item | Required configuration |
| --- | --- |
| Architecture/firmware | x86_64, legacy BIOS (not UEFI); QEMU q35/KVM is the reference |
| Resources | 1 GiB RAM is the reference; disk has 8 GiB virtual capacity |
| System disk | Fresh writable QCOW2 overlay on virtio block |
| Configuration | Read-only ISO CD-ROM labelled `HARNESS_CONFIG`, JSON files at its root |
| NICs | Two virtio NICs with the selected MACs and matching isolated backends; suppress automatic/default NICs |
| Console | Capture serial port 0, 115200 baud, including shutdown; no interactive login |
| Lifecycle | Boot once, no automatic restart; bounded runtime and logs, terminate and reap on failure |

The VM starts the trial automatically, exports a final report, and powers off.
There is no SSH service or unlocked login to configure afterward.

The guest's network settings are not the host isolation boundary. The launcher
must restrict VM-process access to host files, credentials, sockets, devices, and
networks; expose only the required disk/media, KVM device, and scoped transports.
Do not add host directory mounts, container-runtime sockets, or general host
network access. Isolate management sockets from both guests and enforce network
access outside the guest as well. Treat the target and model-directed commands
as untrusted.

In this repository, all host experiment automation runs through sandboxed
`nix build`; do not use host `nix develop` or replace it with direct-host QEMU
execution. A non-Nix deployment must supply and verify an equivalent runner
boundary. This bundle specifies its inputs but does not implement that launcher.

## 5. Collect the result and clean up

The serial stream contains boot diagnostics and exactly one final line beginning
`HARNESS_REPORT:` followed by a JSON object. Retain the raw console log and parse
that line with a bounded JSON parser. Reject missing, duplicate, malformed, or
oversized reports; validate report version and `run_id` against the supplied
manifest. Do not execute report content or interpret it as shell commands.

Require `harness run: COMPLETE`, no `harness run: FAIL`, and successful VM exit.
COMPLETE means successful report export and execution-service completion, not
that the task was solved. Inspect the report's `outcome`; non-submission outcomes
are also recorded. Console markers are operational signals, not proof of isolation.

The guest trial deadline is 720 seconds, report export has 120 seconds, and the
report limit is 1 MiB. The reference launcher allows 900 seconds for the harness
VM and captures at most 4 MiB of console and 64 KiB of QEMU stderr. Use bounded
supervision even when no marker arrives; increasing manifest turn counts does
not extend the guest deadline.

After the harness exits, stop the broker/target environment, verify cleanup,
and terminate/reap any remaining VM processes. Preserve the manifest, network
configuration, report, and logs together. Retire the writable disk after review;
create fresh media and a fresh broker session for the next run. Harness shutdown
alone does not establish that target commands or descendants have stopped.
