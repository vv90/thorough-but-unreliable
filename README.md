# thorough-but-unreliable

A Rust model/tool harness for trials against disposable, Nix-defined targets.
The current deployment pairs a harness VM with an experiment VM running a broker
and a Podman target; real local inference has passed end to end.

See [EXPERIMENTS.md](EXPERIMENTS.md) for the experiment setup procedure. This
README remains the source of truth for build/run commands and runtime limits.
[COMMAND_PROTOCOL.md](COMMAND_PROTOCOL.md) specifies the command API, and
[IMPLEMENTATION.md](IMPLEMENTATION.md) contains implementation rules.

## Harness base image

`nixos/harness-vm.nix` defines the initial x86_64 NixOS guest. The flake exposes
`nixosConfigurations.harness` and `packages.x86_64-linux.harness-image`.
It produces `harness.qcow2`, with its runtime Nix store inside the disk,
legacy BIOS/GRUB boot, an 8 GiB virtual disk, and diagnostics on serial port 0
at 115200 baud. Login is locked and DHCP, IPv6 and forwarding are disabled. A
noninteractive `harness` service account owns `/var/lib/harness`; a boot-time
readiness unit verifies its fixed UID/GID and state-directory access.
The two MAC-matched interfaces use fixed addresses with no default route or
DNS. The image contains the Rust `harness` binary; its boot-time configuration
unit validates the per-run manifest. The base image does not start a trial
automatically. The runnable image adds startup, report handling, and shutdown
through `nixos/harness-run.nix`; both smoke images reuse that module.

Build the standalone runnable image with:

```sh
nix build .#harness-run-image
```

The output is `result/harness-run.qcow2`. It is independent of the selected
target, trial manifest, and host inference socket. At launch, provide a fresh
writable overlay, the `HARNESS_CONFIG` ISO containing `manifest.json`, the two
MAC-matched networks described below, and serial port 0 capture. The existing
sandboxed local runner now uses this image directly.

The trial runs once with a 720-second deadline. Its final report (up to 1 MiB)
is exported as a `HARNESS_REPORT:` JSON line, followed by `harness run: COMPLETE`
or `harness run: FAIL`, then the VM powers off. COMPLETE means a report was
exported with the expected ownership and the execution service completed;
it does not mean the task succeeded. Recorded non-submission outcomes are
accepted. Missing or invalid reports and export failures cannot produce COMPLETE.
The result service has a 120-second deadline. Host supervision must still bound
the VM lifetime and handle missing output or stalled shutdown.

The custom Rust loop calls an OpenAI-compatible inference endpoint, exposes
`execute_target_command` and `submit` to the model, and sends commands through
the experiment broker. Inspect AI and Python are not runtime dependencies of
the harness. [summary.md](summary.md) is a temporary current-state handoff.

On the external Linux builder with KVM available:

```sh
nix build .#harness-image --out-link result-harness
```

Use the external KVM builder for supported image builds and VM checks. The
development container has no `/dev/kvm`; evaluation and non-VM checks run there.
The output disk is `result-harness/harness.qcow2`.

### Run a boot smoke test with KVM

These are the original manual base-image inspection instructions. They launch
QEMU directly on the host, outside the build sandbox. For routine verification,
use the [sandboxed connected check](#run-the-connected-vm-smoke-test-with-kvm)
or [full local inference runner](#full-local-inference-run).

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
legacy BIOS, a serial console, two disconnected virtio network interfaces, and
no host directory:

```sh
qemu-system-x86_64 \
  -enable-kvm \
  -machine q35 \
  -cpu host \
  -m 1024 \
  -drive file=.artifacts/harness-smoke.qcow2,format=qcow2,if=virtio \
  -nic none \
  -device virtio-net-pci,mac=52:54:00:99:01:02 \
  -device virtio-net-pci,mac=52:54:00:99:02:01 \
  -nographic \
  -no-reboot
```

A successful boot reaches the multi-user target, prints the following readiness
message, and displays `harness login:`:

```text
harness readiness: uid=900 gid=900 state-directory=writable
harness network: inference0=10.99.1.2/24 command0=10.99.2.1/30 default-route=none dns=none
```

`-nic none` suppresses QEMU's automatic user-mode network and its implicit NAT.
The two explicit devices have no host network backend in this smoke test. Their
MAC addresses let the guest name and configure them independently of PCI probe
order: `inference0` is `10.99.1.2/24`, and `command0` is `10.99.2.1/30`.

Login is intentionally locked. Exit QEMU by pressing `Ctrl-A`, then `X`. From
another terminal, `pgrep -af harness-smoke.qcow2` shows whether this test VM is
still running. The image derivation evaluates against the pinned Nixpkgs
revision; repeat this smoke test after rebuilding the image.

### Test per-run configuration media

Create a run manifest and place it in an ISO labeled `HARNESS_CONFIG`:

```sh
mkdir -p .artifacts/harness-config
cat > .artifacts/harness-config/manifest.json <<'JSON'
{
  "version": 1,
  "run_id": "smoke",
  "system_prompt": "Use execute_target_command to work on the task, then submit your answer.",
  "task": "Run printf 'harness trial\\n' in the target and submit its output.",
  "max_model_turns": 8,
  "inference": {
    "completion_url": "http://10.99.1.1:11434/v1/chat/completions",
    "model": "qwen3:latest",
    "max_tokens": 1024,
    "connect_timeout_ms": 5000,
    "request_timeout_ms": 120000,
    "max_request_bytes": 1048576,
    "max_response_bytes": 1048576
  },
  "command": {
    "command_url": "http://10.99.2.2:8080/v1/command",
    "connect_timeout_ms": 5000,
    "request_timeout_ms": 60000,
    "max_request_bytes": 65536,
    "max_response_bytes": 262144
  }
}
JSON
nix shell nixpkgs#xorriso --command xorrisofs \
  -quiet \
  -volid HARNESS_CONFIG \
  -joliet \
  -rock \
  -output .artifacts/harness-config.iso \
  .artifacts/harness-config
```

Create a fresh overlay as described above, then add this drive to the QEMU
command:

```sh
-drive file=.artifacts/harness-config.iso,format=raw,media=cdrom,readonly=on
```

The guest mounts the ISO at `/run/harness-config` with `ro`, `nosuid`, `nodev`
and `noexec`. A successful check prints:

```text
harness config: manifest validated run_id="smoke"
```

Without an attached configuration ISO, the guest still boots and prints
`harness config: no configuration media attached`. An attached manifest must be
a JSON object matching the schema above, with every field supplied explicitly.
The typed Rust loader enforces the [manifest rules](#manifest-validation).
Inside the devcontainer, validation can also be run directly as:

```sh
nix run .#harness -- check-config --manifest PATH
```

This expands the early version-1 schema; old two-field manifests are rejected.
Regenerate the configuration ISO when rebuilding the image. The endpoint
addresses, broker port, model, and limits above are configuration-check examples;
`qwen3:latest` is a placeholder, not the selected real-inference model.
The boot check only validates configuration and does not contact
either endpoint, so the disconnected smoke test still works. Before a real run,
choose a model available at the inference endpoint and allow enough command
request time for the broker's execution deadline, cleanup, and reporting.

For convenience, the following wrapper runs the complete build, manifest, ISO,
fresh-overlay and QEMU sequence above:

```sh
./scripts/run-harness-smoke.sh
```

It creates a unique directory under `.artifacts` for each invocation. The
commands in this README remain the source of truth; keep the wrapper synchronized
with them when the workflow changes.

### Run the connected VM smoke test with KVM

This opt-in image runs the packaged harness automatically as UID/GID 900 after
account readiness, network readiness, ISO mounting, and manifest validation.
The ordinary `harness-image` and disconnected smoke script remain unchanged.

On the KVM host with Nix build sandboxing enabled, from the repository root:

```sh
nix build --option sandbox true --keep-failed -L \
  .#checks.x86_64-linux.harness-connected-smoke \
  --out-link result-harness-connected-smoke
```

The convenience wrapper contains those same commands:

```sh
./scripts/run-harness-connected-smoke.sh
```

The check builds its image dependency and compiles/runs the Rust integration test
inside the Nix builder. QEMU, the HTTP fixtures, and nc also execute inside that
build sandbox. No host development shell or host Cargo invocation is required.
The derivation requires a builder advertising the `kvm` system feature. `-L`
shows build logs; `--keep-failed` retains the build directory for diagnosis if
the check fails. A successful result may be reused by Nix for unchanged inputs.

VM run directories use `NAME.YYYYMMDDTHHMMSS.NNNNNNNNNZ.XXXXXX`: a readable run
name, UTC date/time with nanoseconds, and a random suffix for uniqueness. Sorting
by name groups each kind of run first, then orders its runs chronologically.
This applies to `harness-connected`, `harness-smoke`, and `podman-runtime` runs.
Ordinary connected-runner tests also create directories under `.artifacts` for
local fake-process/report checks; these directories do not imply a VM was run.

The test creates a fresh `.artifacts/harness-connected.TIMESTAMP.XXXXXX`
directory inside the build directory, a config ISO, and writable disk overlay. It starts two
loopback HTTP fixtures, then QEMU with the normal static MAC addresses. Each NIC
has a separate restricted user-network backend with an explicit `guestfwd` rule: inference
`10.99.1.1:11434` and broker `10.99.2.2:8080` map to their respective fixture's
ephemeral host port. There are no TAP devices, host network changes, shared host
directories, or real target commands. QEMU uses `nc` for each forwarded connection;
the derivation supplies QEMU, xorrisofs, and nc from pinned Nixpkgs.

The backend networks use /24 masks to accommodate QEMU's internal gateway/DNS
addresses, moved to `.254`/`.253`. The guest retains its original /24 inference
and /30 command configuration, without a default route or DNS. `restrict=on`
limits the user-network backends; this fixture wiring is not the eventual
experiment network or proof of its isolation policy. See
[QEMU user networking and guest forwarding](https://www.qemu.org/docs/master/system/qemu-manpage.html).

The fixed manifest is `tests/fixtures/connected-manifest.json`. The fixtures
require exactly three ordered requests: inference returns one command, the
broker returns predetermined `smoke\n` output, and inference receives that result
and submits. No shell command is executed by the fake broker. The guest saves
`/var/lib/harness/report.json` as UID/GID 900 with mode 0600. A test-only verifier
checks the complete report against `tests/fixtures/connected-report.json`, prints
`harness connected smoke: PASS` or `FAIL`. The shared result service then exports
the report to serial and powers the VM off.

The test requires successful QEMU exit, the guest PASS marker, and the exact
HTTP exchange without missing/extra requests. It fails after 180 seconds, kills
and reaps a stalled VM, and retains `console.log`, `stderr.log`, the ISO and overlay
for inspection. Successful builds publish these under
`result-harness-connected-smoke/artifacts/`; failed builds retain them in the
build directory printed by Nix. The report remains inside the overlay and is
also exported in the serial log. Service startup is bounded to 60 seconds with no
restart; an existing report is never overwritten. Each test creates a fresh overlay.

Ordinary `cargo test` inside the devcontainer checks these same fixtures against
the executable locally, but skips the KVM test. Build the KVM check explicitly:
an ignored test in the ordinary suite is not a successful VM check. This workspace
cannot boot the image without KVM; the external run remains required.

Development commands below, including `nix develop`, run inside the devcontainer.
Host-side build/test automation uses sandboxed Nix derivations.

## Deterministic harness core

`src/harness/model_loop.rs` implements pure transitions: start with a system
prompt, user task, and model-turn budget, then receive a model step, command
step, or finished report. Each step is consumed when its result is supplied.
`src/harness/types.rs` defines the internal messages and outcomes.
`src/harness/driver.rs` is a thin synchronous driver over `ModelClient` and
`CommandExecutor`; tests supply in-memory scripted implementations.
`src/harness/async_driver.rs` provides corresponding asynchronous interfaces and
`run(system_prompt, task, max_model_turns, &mut model, &mut executor).await`.
It awaits one effect at a time and feeds its result into the pure core.
`InferenceClient` implements its model interface; `CommandClient` implements
its command interface. `tests/http_loop.rs` runs the complete loop with both
HTTP clients against scripted localhost endpoints. A shared script checks
request order across both endpoints, exact command text and sequence numbers,
the next inference request's results, and the final transcript. Independent JSON
fixtures cover binary output, truncation, completion provenance, submission,
recoverable rejection, and stopping after command or inference failures.
Unexpected or missing requests fail the test; every scenario has a total timeout.
These tests execute no shell commands and require neither Ollama nor VMs.

Run this integration suite on its own with:

```sh
nix develop --command cargo test --locked --test http_loop
```

The core validates each complete assistant response before executing commands.
Call IDs must be nonempty and unique within that response. Commands execute in
order; each result is appended with its call ID before another model request.
Raw assistant responses retain their original IDs for diagnosis, while dispatched
commands and recorded tool results use a validated `ToolCallId`. Broker request
numbers use the separate `CommandSequence` type; the command client owns sequence
assignment and response correlation.

The transcript retains complete target `ExecutionReport` values, including raw
bytes, truncation, completion provenance, and session usability. The loop stops
immediately on any unusable session, even after exit code zero, retaining the
report and dispatching no further effects. Recoverable input rejection is
recorded for the model; remaining commands can proceed while the session is ready.
Confirmed deadline termination permits continuation only if the session is ready.

Model failures and command-client failures have distinct typed categories and
optional diagnostics. Failure to obtain a valid broker report is distinct from a
report describing failed target execution. These failures terminate immediately
without retries, preserving uncertainty and the completed transcript.
One submission finishes with its answer; multiple
submissions or a mixture of submission and commands are protocol errors. A
response with no tool calls ends as early termination.

The turn budget bounds model calls. Zero permits no calls; commands in the last
allowed response finish while the target remains usable, before the budget
prevents another model request.
A submission or failure on that last turn keeps its specific outcome.
The core has no aggregate history-size limit or incremental event export.
The local VM runner supplies an external wall-clock deadline and bounded logs
and report export; it does not bound all memory used by the loop's history.

Dependency calls catch unwinding panics as explicit failures and stop. Discard
the adapters after such a failure. Process aborts cannot be caught; the driver
does not change the global panic hook. A caught panic payload is deliberately
retained because its destructor could panic too.
The async boundary covers both future construction and polling. Dropping the
run future cancels driving without producing a report; an already dispatched
effect may still complete remotely, so cancellation is not safe to retry blindly.

Run the core's property-based tests and lint checks in the pinned dev shell:

```sh
nix develop --command cargo test --locked
nix develop --command cargo clippy --locked --all-targets -- -D warnings
nix develop --command cargo fmt --check
```

The current pinned toolchain reports two existing `chunks_exact_to_as_chunks`
Clippy warnings in `src/command_protocol/wire.rs` and `src/harness/tests.rs`.
They make the strict lint command fail independently of test results. A focused
check can suppress that known lint with `-A clippy::chunks_exact_to_as_chunks`.

Properties cover ordered/correlated command results, complete transcripts,
deterministic replay, exact submission, early termination, validation before
effects, the model-turn budget, and stopping without retries on dependency
failures. Generated inputs include Unicode text, command completion statuses,
and failures with uncertain completion. Proptest is a development dependency;
test assertion failures are reported by the test framework.

### Inference HTTP client

`src/harness/inference/wire.rs` contains pure request encoding and response
validation. `http.rs` provides `InferenceClient::new(config)` and asynchronous
`complete(&history)`, for use inside a Tokio runtime with IO and time enabled.
The async driver connects this client to the state machine without an extra
runtime or background task; the caller supplies the Tokio runtime.

`InferenceConfig` supplies the full `/v1/chat/completions` HTTP(S) URL, model,
`max_tokens`, connection timeout, total HTTP timeout, and maximum request and
response sizes in bytes. Limits must be positive. URL credentials, query
strings, and fragments are rejected. The client uses HTTP/1.1, disables
redirects, retries, and environment proxies, and retains TLS verification.

Requests contain the conversation, both tool definitions, `stream: false`, and
`n: 1`. Tool arguments and command results are JSON encoded inside their string
fields. `presentation.rs` provides a pure model-facing projection of target
reports: UTF-8 output is text, other bytes are losslessly hex-encoded, and both
carry explicit encoding/truncation metadata. Sequence, completion provenance,
failure category, and session state are included. The original report stays in
the transcript. This projection is separate from the broker wire format.
A history containing a command-client failure or unusable target report cannot
be encoded for another model turn. The serialized
request is checked against its size limit before sending. Response bytes are
bounded while reading, including chunked responses; the total HTTP timeout
covers response body reads too. These bounds do not replace run-wide limits.

Responses must contain one assistant choice at index zero, with a supported
finish reason and well-formed function calls. Tool argument objects require
exactly `command` or `answer`; IDs must be nonempty and unique per response.
Unknown tools, malformed JSON, and token-truncated responses return errors.
Submission/command combinations remain a decision for the pure loop. Transport
errors preserve the possibility that inference already completed; no request
is retried automatically.
Conversion to `ModelFailure` retains typed HTTP status, invalid response,
token-limit, size-limit, configuration, and other categories with diagnostics.

The same Cargo commands above run property tests for wire conversion and size
limits, plus local fake-server tests for request content, parsing, redirects,
disconnects, status errors, body limits, and timeouts. No real Ollama service or
VM is needed for these tests. The separate real Ollama tool-call probe has also
passed with `qwen3.5:9b-q4_K_M` (synthetic command result).
Integration tests drive a command batch through the fake inference server and
in-memory executor, verify the next HTTP request contains the ordered results,
and finish on submission. They also cover failure handling, protocol rejection,
and the final allowed command batch. A property test compares async execution
with the synchronous driver across generated scripts, including suspension,
failures, invalid responses, and turn budgets. Both reports and effect traces
must match. Injected dependency unwinds verify that the async driver stops
during future construction or after suspension.

Protocol and transport references:
[Ollama compatibility](https://docs.ollama.com/api/openai-compatibility),
[Reqwest client configuration](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html).

### Real inference tool-call probe

The host gateway must expose `/run/harness-inference/gateway.sock` to Nix build
users and the build sandbox. The host's sandboxed `/v1/models` probe has passed;
the selected installed model is `qwen3.5:9b-q4_K_M`.

Run on the host, as your normal user:

```sh
bash scripts/run-inference-probe.sh
```

The convenience script runs the following build from the repository root, with
a fresh run ID to prevent reuse of cached inference results:

```sh
nix build --option sandbox true --builders '' --impure --keep-failed -L \
  --file ./nix/inference-probe.nix \
  --argstr runId "$(date +%s%N)-$$" \
  --argstr model 'qwen3.5:9b-q4_K_M' \
  --argstr socketPath /run/harness-inference/gateway.sock \
  --out-link result-inference-probe
```

`--impure` permits evaluation of the local Git flake; build execution remains
sandboxed. Remote builders and substitution of the probe result are disabled.
The script accepts `INFERENCE_MODEL` and `INFERENCE_SOCKET` environment overrides.

The Rust probe in `tests/inference_socket.rs` uses the harness's actual request
encoder, tool definitions, and response parser, with a probe-only Unix-socket
HTTP transport. It asks for exactly one `execute_target_command` call with
`printf 'hello\n'`, supplies a **synthetic** successful report containing
`hello\n`, then requires exactly one `submit` call with answer `hello`.
No target command executes and no VM is started. A failure can indicate schema
incompatibility, failure to follow this small task, token exhaustion, or a
transport error; it is not an isolation verdict.

Requests use the existing harness options (`tool_choice: auto`, non-streaming),
4096 maximum tokens, a 5-second connection timeout, and a 300-second timeout per
request. Request and response bodies are bounded to 1 MiB. There are no retries.
The gateway's upstream timeout must also accommodate local model loading.

Successful builds save both requests, both responses, HTTP statuses, and
`result.txt` under `result-inference-probe/artifacts/inference-probe/`. Failed
builds retain available diagnostic artifacts under `.artifacts/inference-probe/`
in the source directory of the build directory reported by `--keep-failed`.
Transport failures may occur before a response is available; body-read failures
retain the bounded partial response. Use the tracked script above; temporary
helpers under `.artifacts` are not required.

### Full local inference run

With the working host inference socket exposed to Nix builds and KVM available,
run from the repository root on the host:

```sh
bash scripts/run-harness-local.sh
```

The script is a convenience wrapper for:

```sh
nix build --option sandbox true --builders '' --impure --keep-failed -L \
  --file ./nix/harness-local.nix \
  --argstr runId "$(date +%s%N)-$$" \
  --argstr model 'qwen3.5:9b-q4_K_M' \
  --argstr socketPath /run/harness-inference/gateway.sock \
  --out-link result-harness-local
```

1. The build prepares both images and fresh disposable overlays, then boots the
   experiment VM and waits for its broker. The target remains the container
   defined in `targets/podman/smoke.nix`.
2. It boots the harness VM with a generated manifest. Its restricted inference
   NIC forwards only `10.99.1.1:11434` through `nc -U` to the host gateway socket.
   Its command NIC connects directly to the experiment VM's private Ethernet
   stream. QEMU, netcat, and the runner execute inside the build sandbox.
3. The real model is asked to execute `id -u; id -g; pwd; printf 'paired-smoke\n'`
   once, then submit stdout without its final newline. The runner checks the
   actual target result (`1000`, `1000`, `/work`, `paired-smoke`), successful
   parent-observed runtime status, ready session, and matching submission.
4. The harness exports its report and powers off. The runner requests experiment
   shutdown and requires successful broker/target shutdown plus container and
   recorded cgroup removal before reporting PASS.

The wrapper accepts `INFERENCE_MODEL` and `INFERENCE_SOCKET`. Socket paths must
be absolute, at most 107 bytes, and contain only ASCII letters, digits, `/`, `.`,
`_`, and `-`, because QEMU interprets the forwarding command. No shell syntax is
accepted. No host development shell is used. Each invocation has a fresh build
identity, so cached model responses cannot stand in for a new run.

The trial allows two model turns, 4096 tokens each, and 300 seconds per inference
request, without retries. The guest trial has a 720-second deadline; supervisors
also bound VM lifetime and log size. A model that does not follow this small
task fails the check; this is an integration check, not an isolation verdict.

Successful builds retain `harness/report.json`, both VMs' console/stderr logs,
the manifest ISO, and overlays under
`result-harness-local/artifacts/harness-connected.*/`. The report contains the
parsed conversation and command result, not raw inference HTTP responses.
Failed builds retain available artifacts in the build directory printed by
`--keep-failed`; boot failures or timeouts may leave only logs and overlays.
The original deterministic paired-VM smoke check remains available separately.

### Run a supplied trial manifest

The full local inference smoke test has passed on the host. To run a different
task, start with `trials/hello.json`. Edit its `run_id`, `system_prompt`, `task`,
`inference.model`, and `max_model_turns`, then run on the host:

```sh
bash scripts/run-harness-local.sh trials/hello.json
```

With no argument, the script still runs the strict smoke test above. With a
manifest argument, it runs a general trial and records the outcome without
grading the answer. The equivalent build command is:

```sh
nix build --option sandbox true --builders '' --impure --keep-failed -L \
  --file ./nix/harness-local.nix \
  --argstr runId "$(date +%s%N)-$$" \
  --argstr manifestPath "$(realpath trials/hello.json)" \
  --argstr socketPath /run/harness-inference/gateway.sock \
  --out-link result-harness-local
```

The manifest may be outside the repository or untracked. Nix copies it into
the store as a build input; the runner validates it before starting either VM
and puts its original contents in the config ISO. The readable copy is retained
at `config/manifest.json` alongside the VM artifacts. Prompts and other manifest
contents therefore become store/build artifacts; do not put credentials in them.

The supplied manifest is authoritative: `INFERENCE_MODEL` is rejected by the
wrapper when a file is provided; set the model in that file. `INFERENCE_SOCKET`
still selects the host gateway. This runner supports only the existing guest
endpoints: `http://10.99.1.1:11434/v1/chat/completions` and
`http://10.99.2.2:8080/v1/command`. Other addresses are rejected before VM startup.

General trials print `local trial: RECORDED` after collecting a report matched
to the manifest and verifying experiment cleanup. Submission, early termination,
turn-limit exhaustion, model/protocol errors, and command/session failures are
all recorded outcomes. A successful build means report collection and cleanup
worked; inspect `harness/report.json` to assess the task. Missing/invalid reports,
guest crashes, supervisor timeouts, and failed cleanup still fail the build.

The existing 720-second guest trial deadline and 1 MiB report export limit remain
in force. Increasing the turn budget does not extend the wall-clock deadline;
choose request timeouts and turn counts accordingly. No target-environment or
network configuration is selected by the manifest; the launcher selects the
target separately.

### Configuration-repair experiment

Run on the host:

```sh
bash scripts/run-harness-local.sh --target config-repair trials/config-repair.json
```

This selects the `config-repair` target from `targets/default.nix`. The equivalent
build command is:

```sh
nix build --option sandbox true --builders '' --impure --keep-failed -L \
  --file ./nix/harness-local.nix \
  --argstr runId "$(date +%s%N)-$$" \
  --argstr targetName config-repair \
  --argstr manifestPath "$(realpath trials/config-repair.json)" \
  --argstr socketPath /run/harness-inference/gateway.sock \
  --out-link result-harness-local
```

The container contains a small Bash/jq order-summary application, a JSON config,
three orders, and instructions in `/work/README.md`. Its one defect is an input
path: the config points at `/work/data/orders.json`, while the supplied data is
in `/work/fixtures/orders.json`. The model must reproduce the error, inspect
files, edit the config, run `check-orders`, and submit what it changed and the
observed result. The manifest gives it eight model turns within the existing
720-second overall trial deadline.

`check-orders` reports `PASS: 3 orders, total 42` after the intended repair.
This is an in-target check for the model to use; the outer runner still records
the trial without grading its answer or enforcing that the model ran the check.
Inspect the command history and submission in `harness/report.json`.

The target keeps the smoke target's UID/GID 1000, no network, read-only root,
resource limits, and writable 1 MiB `/work` tmpfs. A synchronous preparation
command copies pristine files from the image into that tmpfs before the broker
starts. Changes disappear with the container. Bash, coreutils, find, grep, sed,
and jq are provided; no downloads or Python are needed inside the target.

The launcher defaults to `--target smoke`. Unknown names are rejected by Nix,
and selecting another target requires a manifest. Successful runs also retain
`result-harness-local/artifacts/target.json`, containing the selected name,
image store path/reference, runtime policy, command settings, and preparation
command. The target selection is trusted launcher configuration.

To verify the exercise's preparation, initial failure, and repaired result
without a VM or model, run:

```sh
nix build .#checks.x86_64-linux.config-repair --no-link -L
```

## Target execution interface

`src/target/mod.rs` defines `TargetSession`, the experiment broker's interface
to a prepared container, VM, or other target. Trusted setup binds the target,
execution identity, shell, initial directory, environment, and limits. Each
`execute` request contains only a sequence number and command text. The broker
rejects duplicate/out-of-order requests. Authentication belongs at this boundary
but remains deferred.

An `ExecutionReport` contains a sequence and an `ExecutionOutcome`. Only outcomes
where execution may have started carry raw stdout/stderr with truncation flags;
`NotStarted` cannot contain command output. Completed executions distinguish
parent-observed and guest-reported completion. `Exited` and `Signaled` require
the corresponding observation; `RuntimeStatus` preserves a numeric status when
the runtime loses that distinction. Codes use `u8`, and signals use `NonZeroU32`
with additional platform validation at the adapter boundary.

Session usability is derived from the outcome. `Unknown` and a deadline with
`MayStillBeRunning` cannot carry a readiness override and always require discarding
the session. Known completion or confirmed deadline termination can retain an
adapter's readiness decision. Input rejection has its own error type and preserves
a healthy session; session failures are terminal. Diagnostics remain separate
from command output. These types prevent contradictory reports; adapters still
must establish the reported facts and enforce their own lifecycle.

Adapters must launch fresh shells with EOF on stdin, preserve target filesystem
state, apply the configured output/deadline/descendant policy, and never retry
automatically. Uncertain completion or protocol failure makes a session unusable;
subsequent requests must be rejected without dispatch. Dropping an execution
future requires discarding the session and notifying supervision. If termination
cannot be confirmed at a deadline, supervision must end the trial.

The trait, data types, and Podman adapter are implemented, with a real-runtime
VM check described below. The experiment service prepares and removes the
selected Podman target, and the paired runner supervises both VMs. Other target
backends and a general deployment controller remain unimplemented. The separate
command protocol maps these internal types to its wire schema.
The harness's `async_driver::CommandExecutor` remains the client-side interface;
`src/broker` translates between HTTP and `TargetSession`.

### Podman adapter

`src/target/podman` contains pure logic for one prepared container. `Config::new`
validates trusted `Settings`: an absolute Linux Unix socket path, a full 64-digit
lowercase hexadecimal container ID, numeric UID/GID, absolute shell and working
directory paths, explicit environment overrides, and positive command/JSON/capture
limits and deadline. Setup must still establish socket permissions, container
identity, resource policy, and the container's base environment. Environment
overrides supplement that base; nothing expands variables from the broker.

`Session::begin` rejects NUL-containing or oversized commands before reservation.
It passes accepted text unchanged as the third argument in `[shell, "-c", command]`.
The generated exec configuration uses EOF stdin, separate stdout/stderr, no TTY,
and no privilege escalation. It targets Podman's unversioned Docker-compatible
exec endpoints. The API mapping follows Podman's
[exec handlers](https://github.com/containers/podman/blob/main/pkg/api/server/register_exec.go)
and [stream framing](https://github.com/containers/podman/blob/main/pkg/bindings/containers/attach.go).

Execution stages consume one another: `Create` → `Start` → `Capture` → `Inspect`.
Each borrows the session exclusively, preventing overlapping attempts. Reservation
immediately marks the session unusable; dropping or forgetting any unfinished
stage leaves it that way. Only clean output EOF followed by an inspection proving
the matching exec stopped in the matching container restores readiness. A pending
inspection permits another read under the same deadline, never another start.

The stream decoder accepts arbitrary chunk boundaries, retains separate output
prefixes, and discards excess bytes while maintaining frame alignment. Frame
lengths never determine allocations. Partial EOF, invalid framing, runtime error
frames, malformed JSON, correlation failures, and effect failures terminate the
session while preserving already captured output. Inspection produces
`RuntimeStatus`, including for code 137; it never guesses a termination signal.

The chosen descendant policy permits background processes to persist within a
trial. Descendants holding output streams open remain subject to the command
deadline. Readiness is permission to continue, not evidence that descendants
have stopped or that isolation held. A failure after start may have been sent is
uncertain; a deadline then reports `MayStillBeRunning` and requires trial teardown.

`PodmanTargetSession::new(config, supervisor)` wraps this core and implements
`TargetSession`. The supervisor argument is a Tokio oneshot sender carrying
`SupervisionEvent`. Construction performs no IO. Execution connects only to the
configured Unix socket, with no network fallback, environment proxies, redirects,
or retries. Each create/start/inspect request owns a fresh HTTP/1.1 connection.
Connection drivers are polled alongside their requests; no tasks are detached.

One deadline covers connection setup, request/response IO, output draining,
inspection polling, and final decoding. Pending inspections wait 25 ms between
reads under that same deadline. The broker watchdog must be longer than the
adapter deadline to let the adapter return its partial output and terminal report.
JSON body reads use the configured byte limit for fixed-length, chunked, and
EOF-delimited responses. Header parsing has a 32-header/16 KiB buffer bound.
Responses require unencoded HTTP/1.1 JSON or a validated `101` TCP upgrade.
Any output bytes already read with the upgrade headers are preserved.

Keep the supervision receiver alive throughout the trial. A terminal report
sends `SessionUnusable`; dropping a polled, unfinished execution sends
`ExecutionCancelled`. Disposing of the adapter without an earlier notice sends
`SessionDropped`. Only one terminal notice is sent. Closing the receiver prevents
new commands. The supervisor must handle notices or unexpected channel closure
by arranging trial cleanup. These notifications and socket closure do not prove
that the command or its descendants stopped; no target teardown is implemented
by the adapter itself.

`tests/podman.rs` uses real Unix sockets with scripted runtime endpoints. It covers
HTTP/upgrade failures, response bounds, partial output, one shared deadline,
cancellation at every IO stage, session reuse rules, and supervision notices.
It also connects the existing harness client through the broker and actual adapter
to the fake runtime. No Podman daemon or target command runs in these tests.
Run them inside the devcontainer:

```sh
nix develop --command cargo test --locked --lib target::podman
nix develop --command cargo test --locked --test podman
```

### Experiment VM configuration

[`nixos/experiment-vm.nix`](nixos/experiment-vm.nix) defines the shared NixOS
experiment VM foundation: a disposable BIOS-booted qcow2 image, serial logs,
locked logins, and Podman with a root-only Unix socket. The flake configuration
`nixosConfigurations.experiment` imports the service module as well, so its
image includes the default smoke target and broker. Build it on the host with:

```sh
nix build --option sandbox true -L .#experiment-image --out-link result-experiment
```

The image is `result-experiment/experiment.qcow2`.
[`nixos/experiment-service.nix`](nixos/experiment-service.nix) adds target
preparation, the standalone broker, and command networking to that foundation.
On boot, `experiment-target.service` loads the selected container archive,
creates one restricted target, records its ID and cgroup under `/run/experiment`,
runs any target-specific `prepareCommand`, and writes a mode-0600 broker
configuration there. `experiment-broker.service`
starts after preparation and runs
`experiment-broker --config /run/experiment/config.json` as root in the VM,
with access to the root-only Podman socket. The target receives no such access.

The experiment NIC must have MAC `52:54:00:99:02:02`; NixOS names it `command0`
and assigns `10.99.2.2/30`. The broker listens on `10.99.2.2:8080`, with TCP 8080
allowed on that interface. The harness peer is `10.99.2.1/30`. There is no
configured gateway, DNS, DHCP, or IP forwarding. Attach both command NICs to an
isolated link when pairing VMs; the paired VM check below supplies that link.
Without the expected NIC/address, preparation fails after a bounded wait.

Serial logs and `journalctl -u experiment-broker -u experiment-target` show the broker's
`experiment broker: ready listen=10.99.2.2:8080` message after binding.
`systemctl stop experiment-broker` sends SIGTERM, allowing accepted work to drain.
When the broker exits, its target service becomes unneeded and stops. Failed
preparation also invokes cleanup: remove the target and check that its recorded
cgroup has disappeared. Cleanup
failure is a service failure; it does not certify termination. The service does
not restart automatically. Discard the VM after the trial and boot a fresh one
for the next trial.

Command deadlines are 30 seconds, with a 35-second broker watchdog and 5-second
HTTP read/write limits. Each stream retains at most 64 KiB; commands are at most
4096 bytes. Systemd caps broker runtime at 15 minutes and stop operations at
60 seconds. These are the defaults in `experiment-service.nix`; the local-trial
derivation extends the broker runtime to 1300 seconds.
When connecting a live harness, set its command URL to
`http://10.99.2.2:8080/v1/command`, request timeout to 60000 ms, and response
limit to 1048576 bytes so it can receive the broker's bounded reports.
Authentication remains deferred.

To verify the deployed service on the KVM host:

```sh
nix build --option sandbox true --keep-failed -L \
  .#checks.x86_64-linux.experiment-service \
  --out-link result-experiment-service
```

This sandboxed check boots the live configuration with an isolated command NIC
and a guest-side test client. It verifies actual command execution, delivery of
an uncertain deadline report, broker exit, and systemd removal of the target
and its cgroup. The deliberate timeout makes the broker unit fail as expected;
the check passes only after the target unit finishes cleanup successfully.
It retains logs and the overlay under
`result-experiment-service/artifacts/`. This particular check uses a client in
the experiment VM; the paired check below tests the separate harness VM.

### Verify the harness and experiment VMs together

Run on the KVM host:

```sh
nix build --option sandbox true --keep-failed -L \
  .#checks.x86_64-linux.paired-vm \
  --out-link result-paired-vm
```

The runner executes inside the Nix build sandbox. It boots fresh writable
overlays of `experiment-image` and `harness-paired-smoke-image`, waits for broker
readiness, and connects their command NICs through a private QEMU Unix socket.
This carries Ethernet directly between the two guests: no host bridge, TAP,
port forwarding, or shared directory is needed for command execution.

Only inference is simulated. The harness inference NIC uses a restricted QEMU
network forwarding its one inference endpoint to a fake model inside the build
sandbox. The model requests `id -u; id -g; pwd; printf 'paired-smoke\n'` and
accepts only the exact returned tool report before requesting submission.
The command goes through the real HTTP broker and Podman adapter in the
experiment VM. Expected output is UID/GID 1000, `/work`, and `paired-smoke`,
with runtime status 0 and `parent_observed` provenance.

The harness verifies its complete report and its ownership/mode, then powers
off. The runner requests ACPI shutdown of the experiment VM through a private
QEMU monitor and requires successful broker shutdown and target cleanup in its
serial log. Both QEMU processes have bounded runtime and output capture;
failures trigger process cleanup and retain diagnostic logs.

Successful builds retain the manifest, ISO, both overlays, and separate
`harness/` and `experiment/` logs under `result-paired-vm/artifacts/`.
Failed builds retain artifacts in the build directory printed by `--keep-failed`.
Fixtures live in `tests/fixtures/paired-{manifest,report}.json`; the runner is
`tests/connected_vm/paired.rs`. The ordinary harness image still does not start
trials automatically.

This proves the functional path from the harness through the broker to the
container and back, plus orderly cleanup. It does not connect to Ollama or
establish an isolation verdict. Authentication remains deferred.

[`nixos/podman-runtime-smoke.nix`](nixos/podman-runtime-smoke.nix) imports the same
foundation and adds the selected target, test execution, cleanup, and automatic
poweroff. The existing Podman check below continues to exercise that setup.

### Define a target environment

The default Podman target is defined in
[`targets/podman/smoke.nix`](targets/podman/smoke.nix). It is a Nix function taking
`pkgs` from the pinned Nixpkgs and returning:

| Field | Purpose |
| --- | --- |
| `image` | Container archive built with `dockerTools.buildLayeredImage`: packages, initial files, user, working directory, environment, and initial process. |
| `imageReference` | Local image name and `latest` tag used after loading the archive. |
| `runArgs` | List of Podman runtime arguments defining networking, filesystem access, privileges, and resource limits. Each entry is one argument, not a shell fragment. |
| `command` | Broker execution settings: `uid`, `gid`, absolute `shell` and `workdir`, and `environment` as a list of `[name, value]` pairs. Duplicate names are rejected. |
| `prepareCommand` (optional) | An argv list run with `podman exec` as the container's configured user before the live broker starts. Failure aborts setup and triggers cleanup. |

To define a target:

1. Edit this file, or create another definition under `targets/podman/`. Set the
   packages and initial files in `image`, then its initial process and environment
   in `image.config`.
2. Set the runtime policy in `runArgs`. The current fixture has no network, a
   read-only root, no capabilities, and a bounded writable `/work` tmpfs.
3. Register the definition in `targets/default.nix` and select it using
   `scripts/run-harness-local.sh --target NAME MANIFEST.json`. The standalone
   default remains `targetEnvironment` in the flake's
   `nixosConfigurations.experiment.specialArgs`. The service smoke configuration
   selects its target separately in `nixosConfigurations.experiment-smoke`;
   the older adapter fixture uses `nixosConfigurations.podman-runtime-smoke`.
   Run the relevant verification command after changes.

[`nixos/podman-runtime-smoke.nix`](nixos/podman-runtime-smoke.nix) consumes this
definition, loads the archive, and shell-escapes the arguments before starting
the container. VM setup owns the container name, `--pull=never`, lifecycle,
cleanup, and observation of its ID and cgroup. Target definitions are trusted
Nix configuration; their runtime arguments can change isolation policy.

This target is still a compatibility fixture. Its command-adapter settings and
expected results live in `tests/podman_runtime.rs`; changes to its user, paths,
or tools may require updating that test. The standalone service consumes the
target's `command` settings directly. This definition is specific to Podman; future VM or
physical-machine targets can have their own definitions.

### Verify the adapter against real Podman

On the KVM host, from the repository root:

```sh
nix build --option sandbox true --keep-failed -L \
  .#checks.x86_64-linux.podman-runtime \
  --out-link result-podman-runtime
```

The builder must have Nix sandboxing enabled and advertise the `kvm` system
feature. No host development shell, Podman socket, or Cargo invocation is needed.
The check builds a separate disposable VM image and a small container image from
pinned Nixpkgs. It boots with no network interfaces or shared host directories;
the container archive and Rust test executable are already in the VM image.
The ordinary harness images and connected smoke check are unchanged.

Inside the VM, trusted setup loads the archive without a registry pull and starts
one container with UID/GID 1000, no network, a read-only root filesystem, no Linux
capabilities, no privilege escalation, and bounded memory, CPU and processes.
`/work` is a writable, size-limited tmpfs. Rootful Podman and its root-only Unix
socket stay in the experiment VM; the target never receives the socket or a
mount of the VM's directories. This is a runtime compatibility fixture, not yet
the chosen production target policy. See Podman's [run options](https://docs.podman.io/en/latest/markdown/podman-run.1.html).

`tests/podman_runtime.rs` runs this path entirely inside the VM:

```text
command client → loopback HTTP broker → Unix-socket adapter → Podman → container
```

It requires the configured identity, working directory and environment; EOF
stdin; separate binary stdout/stderr; nonzero completion status; filesystem
persistence with fresh shell state; bounded output with truncation; and successful
reuse after earlier commands. Its last command starts a child and waits past the
five-second adapter deadline. The report must retain partial output and declare
`MayStillBeRunning`. The supervisor must receive the correlated terminal notice,
remove the container, and verify both container absence and disappearance of the
target's cgroup, observed from the VM's procfs. The broker must terminate the
unusable session. Cleanup also runs on test failure; VM shutdown is the final
fallback. The adapter itself still does not manage target lifecycle.

The guest prints `podman runtime smoke: PASS` only after assertions and cleanup
succeed, then powers off. `tests/podman_vm.rs` requires that marker, no FAIL
marker, and a successful QEMU exit. It bounds the run to 300 seconds and kills
and reaps a stalled VM. Successful builds retain `console.log`, `stderr.log`
and the writable overlay under `result-podman-runtime/artifacts/`; failed builds
retain their directory via `--keep-failed`. Nix may reuse an existing successful
result for unchanged inputs.

Both new tests are ignored by ordinary `cargo test`: it compiles them but does
not establish real-runtime success. Run the explicit KVM check above. This
workspace has no KVM device. The image was built and the guest assertions passed
under QEMU software emulation (TCG), including verified cleanup and clean VM exit.
The user also confirmed that the sandboxed KVM Podman check passed on the host.
The check does not exercise a model, the harness VM, a production broker service,
paired VM networking, or an isolation evidence verifier.

## Command protocol

[COMMAND_PROTOCOL.md](COMMAND_PROTOCOL.md) defines the version-1 HTTP/JSON
harness-to-broker contract: `POST /v1/command`, strict tagged execution reports,
hex-encoded output, and one outstanding command with no retries.
`src/command_protocol` implements pure bounded request/report codecs, response
correlation, and sequence tracking starting at 1. Properties cover lossless
conversion, exact body limits, and terminal sequence states.

`src/harness/command` supplies `CommandClient` and `CommandConfig`. Configuration
specifies the full HTTP(S) `/v1/command` URL, connection and total-request
timeouts, and request/response JSON byte limits. The total timeout must allow
the broker's separately configured execution deadline, cleanup, and reporting.
The client sends HTTP/1.1 with no redirects, retries, or environment proxies;
HTTPS uses verified TLS through rustls. It requires HTTP 200 and exactly one
`Content-Type: application/json` header without parameters (case-insensitive).
Response accumulation is bounded even without `Content-Length`.

The client assigns sequences starting at 1 and returns the complete correlated
report. Every client error ends its session; an unusable target report is returned
intact and prevents further dispatch. Cancelling a pending request also prevents
reuse, but cannot stop remote execution. Do not replace the client within the
same run to reset its sequence state. Fake-broker tests cover these behaviors,
body limits, malformed replies, redirects, disconnects, and timeouts.
Authentication remains pending.

## Experiment broker

`src/broker` provides `serve(listener, session, config, shutdown)` for one
prepared `TargetSession`. The caller supplies the listener, validated byte and
connection limits, and read/adapter/write timeouts. Hyper handles HTTP/1.1
framing; the broker bounds body reads, checks the shared protocol, and reserves
one command at a time. A separate session-owner future holds the adapter so an
HTTP disconnect cannot cancel accepted execution. Busy requests are rejected,
without queueing or retrying them.

Only a correlated, bounded report from a ready session permits another command,
after its response is written locally. Invalid adapter reports, unusable reports,
and delivery failures end the session. The adapter watchdog produces a
`MayStillBeRunning` deadline report when it expires. An unwinding adapter produces
an unknown-completion report. Neither establishes that target execution stopped.

Signal shutdown and await the server to drain accepted work. The caller must
supervise target cleanup; dropping the server future is not graceful shutdown.
Successful local writes cannot prove the client received a report. Full bounds,
HTTP failure behavior, and lifecycle obligations are in
[COMMAND_PROTOCOL.md](COMMAND_PROTOCOL.md).

The broker library backs the standalone `experiment-broker` executable deployed
above. Its own tests use fake `TargetSession` implementations;
`tests/podman.rs` also exercises it with the Podman adapter. The original
connected VM smoke test uses a scripted command endpoint; the paired VM check
uses the deployed broker and real target. Run the broker tests inside the devcontainer:

```sh
nix develop --command cargo test --locked --test broker
```

They exercise the real harness client against the broker, ordered admission,
malformed and oversized input, chunked bodies, overlapping requests, disconnects,
graceful shutdown, connection/read limits, adapter watchdogs/unwinds, and invalid
reports. Pure properties check exact byte bounds, non-mutating rejected appends,
report correlation, and configuration limits. The separate Podman VM check above
exercises the same broker with its adapter and an actual container runtime.

## Run the harness

Inside the devcontainer, the executable supports running one trial from a
validated manifest with reachable test endpoints:

```sh
nix run .#harness -- run --manifest PATH > report.json
```

It creates a current-thread Tokio runtime, constructs both HTTP clients using the
manifest settings, and drives the existing async loop. Both endpoints must be
reachable from wherever the executable runs. No retries or automatic restart are
added. The intended deployment is the `harness` account inside the harness VM.
Automatic startup is supplied by `nixos/harness-run.nix`, shared by the standalone
runnable image and both smoke images. Use the sandboxed local runner for
host-side experiments.

The command writes one final JSON object followed by a newline to stdout.
Operational errors before a report exists, or while writing it, go to stderr.
Exit status `0` means the model submitted an answer and the report was written
and flushed successfully. Status `1` means any other outcome or an operational
failure. Submission does not establish isolation success; that requires the
external verifier. Keep the JSON report even when the exit status is nonzero.

The report has these top-level fields:

```json
{
  "version": 1,
  "run_id": "example",
  "outcome": {"kind": "submitted", "answer": "..."},
  "history": []
}
```

The actual `history` contains every recorded message, in order. System/user
messages have `role` and `content`; assistant messages have `role`, nullable
`content`, and `tool_calls`. Each call contains its exact `id` and a `tool` object:
`{"kind":"execute_target_command","command":"..."}` or
`{"kind":"submit","answer":"..."}`. Tool messages have `role`, `call_id`, and
a tagged `result`: `{"kind":"execution_report","report":...}` or
`{"kind":"command_client_failure","error":...}`.

Execution reports use the lossless presentation described above: raw output is
represented as UTF-8 or hex with explicit encoding/truncation, preserving sequence,
completion provenance, session usability, and uncertain outcomes. Failures retain
their typed `category` object, nullable `diagnostic`, and `completion_unknown`.
Category payloads include HTTP `status`, byte `limit`, unsupported tool `name`, or
mismatched `expected`/`received` sequences where applicable.

Outcome kinds are `submitted`, `early_termination`, `model_turn_limit`,
`protocol_error`, `model_failure`, `command_client_failure`, and
`target_session_unusable`. Failure outcomes include their `error`; command-client
failure and unusable-session outcomes identify the `call_id`. Protocol errors
distinguish empty/duplicate call IDs, multiple submissions, and mixed submission
and commands. The final report schema is separate from the broker protocol.

Reports are emitted only after the loop finishes; this is not a crash-recovery
log. Output failure can leave a partial JSON document. Process termination can
lose the report while a remote command continues; do not restart the same trial
blindly. The local VM runner collects the final report through the serial
console, with a 1 MiB export limit. Aggregate in-process history limits and
incremental crash-recovery output remain unimplemented.

Run the executable integration tests locally with:

```sh
nix develop --command cargo test --locked --test cli_run
```

They create temporary manifests, launch the actual binary, and check requests
against fake inference/broker endpoints, stdout reports, stderr, and exit status.
They execute no target commands and require no VM or Ollama. The existing
`scripts/run-harness-smoke.sh` rebuilds and boots the package, but its disconnected
VM only runs `check-config`. The separate connected smoke test above exercises
automatic startup and the HTTP exchange inside the guest.

New source files must be added to Git before using `.#harness` or the smoke
script, because Git-backed flakes omit untracked files. During development,
`nix build "path:$PWD#harness" --no-link` also includes untracked source files.

## Manifest validation

`Manifest`, `InferenceSettings`, `CommandSettings`, and `TransportLimits` have
private fields and read-only accessors. Positive integer limits use nonzero types;
URLs are parsed, and timeouts become `Duration` values. No HTTP client is built
and no endpoint reachability is checked during validation.

- `version` must equal `1`; `run_id` must be nonempty.
- `system_prompt`, `task`, and `inference.model` must contain non-whitespace text.
  Text is preserved exactly, including whitespace, Unicode, and line breaks.
  Prompts are inline strings, with no file loading or environment substitution.
- `max_model_turns`, `inference.max_tokens`, and each timeout in milliseconds
  must be integers from `1` to `4294967295`. The timeout ceiling is approximately
  49.7 days, allowing pure validation without reading a platform clock.
- Each connection timeout must not exceed its total request timeout. The total
  includes response-body reading. The broker's execution deadline is configured
  separately, so its relationship to the command timeout cannot yet be checked.
- Byte limits must be positive integers no larger than the platform's `isize::MAX`
  (`9223372036854775807` on the x86_64 harness). These are JSON-body limits,
  including encoding overhead; validation does not guarantee that a particular
  conversation or captured command output fits them.
- URLs must use HTTP(S), have a host, and have exactly `/v1/chat/completions` for
  inference or `/v1/command` for commands after URL parsing. Credentials, queries,
  and fragments are rejected. Standard URL normalization applies. Authentication
  remains deferred.

All fields are required. Nonobjects (including positional arrays), duplicate
fields, unknown fields, incorrect types, and trailing JSON are rejected at every
object level. `from_slice` performs pure validation; file/reader methods provide
the IO boundary. These loaders cap the complete manifest at 1 MiB, including JSON
syntax and inline text. Direct Serde deserialization enforces the same schema and
semantics, but its caller must bound the source bytes. Reader failures, allocation
failures, and dependency unwinds at the loader boundaries return explicit errors.

## Development container

`.devcontainer/devcontainer.json` uses the development image built by this
repository's flake and pinned inputs. Development is performed inside this
container, including Rust tests, Nix evaluation, and non-VM package builds.
Host-side experiment runners execute through sandboxed `nix build`.

### Development image settings

- Image reference: `localhost/thorough-but-unreliable-dev:latest`. Use the
  `latest` tag for now; rebuild, load and recreate after image changes.
- The development image includes the Rust compiler, Cargo, rustfmt, Clippy and
  rust-analyzer from the flake's pinned Nixpkgs revision.
- A dedicated Codex state volume is persisted; no tool-cache volumes are added.
  No full home, host caches, host credentials, runtime sockets or devices are mounted.
  The source checkout is deliberately writable.

## Build and load

From the repository root, on the host or designated builder with Nix and
rootless Podman:

```sh
nix build .#devImage
podman load < result
```

Nix creates `result` as a symlink to the image archive in the Nix store. Podman
loads that archive as `localhost/thorough-but-unreliable-dev:latest`, matching the
devcontainer configuration. Then open or recreate the devcontainer in your
editor; loading an image does not update an existing container.

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
Check volume ownership and login persistence when provisioning a new instance;
rebuilding the image does not validate or modify an existing volume.

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
in the private store. These are checks for a new instance; package builds and
Rust development already run in the current devcontainer.

## Deployment-specific checks and remaining work

- Verify HTTPS with Nix, curl and the agent; some clients use their
  own certificate-discovery rules. Do not disable TLS verification.
- Check editor server requirements when changing editors or hosts; host-distro
  library paths may not exist in this Nix userspace.
- Codex is supplied by the pinned Nixpkgs revision.
  Only its dedicated state directory is persisted, not all of `/home/dev`.
- Networking is the runtime default. It does not block host/LAN access. A
  dedicated host-managed network and policy are separate deployment work.
- Editor credential/SSH-agent forwarding must be disabled or explicitly scoped
  in editor settings and verified in the resulting container.
- Authoritative package checks and VM integration run externally against a
  recorded source snapshot, without exposing host Nix/libvirt/Podman sockets.
- Add scoped cache volumes only when their benefit justifies persistence, then
  decide their retention and quotas.

Editing these definitions does not update a running container. Rebuild, load,
and recreate it to apply image changes.
