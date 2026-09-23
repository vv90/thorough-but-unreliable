# thorough-but-unreliable handoff

Current state as of 2026-09-23. This is a temporary handoff, not an architectural
contract. README is authoritative for commands; EXPERIMENTS.md explains how to
set up an experiment. Follow AGENTS.md and IMPLEMENTATION.md.

## Working direction

Use a custom Rust model/tool loop, with pure logic and thin effect boundaries,
validated types, property tests for semantic invariants, and explicit errors.
Do not use production panics as an escape hatch. Authentication and automatic
trial expectations were explicitly deferred by the user.

Work one discussed increment at a time. Development stays in the devcontainer.
Host experiment automation must use sandboxed nix build, never host nix develop.
The current devcontainer has no /dev/kvm and cannot access the host inference
socket. Do not claim a real VM/model run from local unit or fake-server tests.
Ordinary connected-runner tests create five artifact directories; the user
recently cleared clutter and noticed this. Avoid unnecessary runs for doc edits.

## Current deployment

The sandboxed local runner starts two disposable QEMU/KVM guests:

- Harness VM: Rust loop, inference client, command client; UID/GID 900.
- Experiment VM: root-only Podman socket, standalone experiment-broker service,
  one restricted target container, preparation and cleanup services.

The command NICs share a private QEMU Unix Ethernet stream. The harness is
10.99.2.1/30 and the broker is 10.99.2.2:8080. The harness inference NIC is
10.99.1.2/24. QEMU forwards only guest 10.99.1.1:11434 through nc -U to the host
nginx gateway at /run/harness-inference/gateway.sock. No host gateway TCP listener
is required. The host gateway forwards to local inference; the installed model
used in the examples is qwen3.5:9b-q4_K_M.

The host gateway and Nix daemon configuration are external to this repository.
The user selected optional global sandbox exposure of the socket for all builds,
not per-derivation approval hooks. The socket is nginx:nixbld mode 0660 in a
2750 directory; sandbox access and a real tool-call probe were confirmed.

The base harness image only checks readiness/configuration. The standalone
harness-run-image uses nixos/harness-run.nix to run the harness automatically,
export a bounded final JSON report
over serial, and powers off. The runner then requests experiment shutdown and
requires successful broker/target shutdown plus container/cgroup removal.

Both smoke images import the same execution module and add fixture verification
before shared report export/shutdown. The local runner directly uses harness-run;
the old nixos/harness-local.nix override was removed. Serial completion markers
are harness run: COMPLETE/FAIL. COMPLETE requires successful report export but
does not grade the task. The runnable image accepts recorded non-submission
outcomes and still requires fresh disposable overlays.

Harness boot networking now accepts optional network.json beside manifest.json
on the HARNESS_CONFIG ISO. Version 1 selects MAC and static IPv4 CIDR for the
inference and command roles; omission preserves the previous defaults. Pure
Rust validation lives in src/network.rs, with bounded file IO in the
harness-network-config binary. nixos/harness-network.nix applies it after udev
settles and media mounting, before readiness and trial startup; networkd is
disabled in the harness. Invalid files fail closed. There are still exactly two
NICs, no DHCP/DNS/default route/IPv6/forwarding, and disjoint subnets. The existing
launchers and experiment image retain their fixed topology; custom deployments
must configure matching peers and manifest URLs themselves. Host VM verification
of the default layout passed. The new paired-vm-custom-network Nix check tests
alternate MACs and 192.168.40.2/24 + 192.168.50.1/30 addresses using exactly the
same harness image as paired-vm. Only the experiment peer image is configured
differently, via experimentCommandNetwork in experiment-service.nix. The check
supplies network.json and alternate manifest URLs, uses scripted inference,
and requires custom readiness, the exact report, successful export, and clean
experiment shutdown. Its host KVM run is still needed.

Limits: 720 seconds for the guest trial, 120 seconds for report export service,
900 seconds for harness VM supervision, 1200 seconds for paired experiment
supervision, and 1300 seconds for the local experiment broker lifetime.
Reports export up to 1 MiB; VM console logs are bounded to 4 MiB and stderr to
64 KiB. The broker uses a 30-second command deadline, 35-second watchdog, 4096
command bytes, and 64 KiB capture per stream. No automatic retries.

## Running and defining experiments

Host commands:

- Strict real-inference smoke: bash scripts/run-harness-local.sh
- General trial: bash scripts/run-harness-local.sh trials/hello.json
- Repair task: bash scripts/run-harness-local.sh --target config-repair trials/config-repair.json
- Inference-only probe: bash scripts/run-inference-probe.sh

The wrapper passes a fresh run ID into nix/harness-local.nix to prevent reuse of
cached trial results. With a supplied manifest, its model/prompt/turn budget are
authoritative; INFERENCE_MODEL is rejected by the wrapper. INFERENCE_SOCKET
remains configurable. The manifest may be untracked/outside Git and is copied
into the Nix store. New target Nix files/assets must be added to Git.

targets/default.nix registers smoke and config-repair. Target definitions return
an image, imageReference with latest tag, runArgs, command settings, and optional
prepareCommand. nix/harness-local.nix passes the selected targetEnvironment into
the experiment VM using extendModules.specialArgs. No target selection appears
in model tool calls or in the task manifest.

The experiment service records the container ID/cgroup, executes prepareCommand
as the container's configured user, then publishes broker configuration. Any
preparation failure invokes existing cleanup. Rootful Podman stays in the
experiment VM; the target gets neither its socket nor host directory mounts.

Both targets use UID/GID 1000, no network, read-only root, no capabilities,
resource limits and a writable 1 MiB /work tmpfs. config-repair adds Bash/jq
orders-summary, check-orders, inspection tools, and a pristine seed copied to
/work before broker startup. The sole defect is config.json pointing at
/work/data/orders.json instead of /work/fixtures/orders.json. The expected
in-container check output is PASS: 3 orders, total 42. The trial allows eight
model turns; no outer automatic grader was added.

General trials validate report identity/prompts/turn count and terminal envelope,
then retain all recognized outcomes, including non-submission. Only in supplied
manifest mode does systemd accept harness CLI exit 1 as a recorded outcome.
Absent/invalid reports, crashes, timeouts, and cleanup failures still fail.
RECORDED is not task success. Strict smoke mode still verifies exact known
command output, completion provenance, and submission.

Successful results contain a timestamped harness-connected run directory with
harness/report.json, both VM logs, original config/manifest.json, ISO and overlays.
artifacts/target.json records the target name, image path/reference, runtime
restrictions and execution/preparation settings. Run directory names start with
the readable type, then UTC YYYYMMDDTHHMMSS.NNNNNNNNNZ and a random suffix.
Failed-build artifacts are retained via --keep-failed, not published as success.

## Code map

- src/harness/model_loop.rs and types.rs: pure loop transitions and typed outcomes.
- src/harness/{driver,async_driver}.rs: synchronous/async effect boundaries.
- src/harness/{inference,command}/: bounded HTTP clients; no proxy, redirects or retries.
- src/harness/{runner,report}.rs: manifest-driven execution and final report.
- src/manifest.rs and manifest/: bounded strict version-1 schema and validated types.
- src/command_protocol/: strict HTTP/JSON wire codecs and command sequencing.
- src/broker/ and src/bin/experiment-broker.rs: server, admission, session ownership,
  configuration and signal-driven draining.
- src/target/podman/: pure execution stages, bounded stream decoding, thin Unix IO.
- nixos/: base guests, trial extensions, experiment service and runtime checks.
- nix/harness-local.nix and nix/inference-probe.nix: parameterized sandboxed runs.
- targets/: named Podman definitions and initial files; trials/: supplied manifests.
- tests/connected_vm.rs and connected_vm/{paired,local,trial}.rs: VM supervision,
  smoke/general assessment, collection and cleanup verification.

## Validation and remaining work

User-confirmed host results: real Podman sandboxed check, real inference probe
with a synthetic command result, and full real-inference paired-VM smoke test.

The config-repair archive build and deterministic Nix exercise check passed.
That check uses the actual preparation command, verifies writable config and
initial failure, repairs the path, and checks the application/checker result.
Selected-runner evaluation, invalid-target rejection, manifest validation,
ShellCheck, formatting and launcher argument forwarding also passed. No host
model run of config-repair has been confirmed in this conversation.

Rust unit/property tests and fake HTTP/Unix-server integrations cover the loop,
codecs, clients, broker and adapter. KVM tests are opt-in; ignored tests are not
evidence of a successful VM run. The newer pinned toolchain previously reported
two unrelated chunks_exact_to_as_chunks Clippy warnings in
src/command_protocol/wire.rs and src/harness/tests.rs; focused checks suppressed
that existing lint rather than modifying unrelated code.

Next practical verification: run config-repair on the host and inspect the
actual commands, check output and submission. Automatic expectations remain
deferred. Authentication, additional target backends, external isolation
evidence/verdicts, incremental crash-recovery output, and aggregate in-process
history limits remain unimplemented. Functional success and cleanup alone do
not establish isolation.
