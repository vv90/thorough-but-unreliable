# thorough-but-unreliable

## Harness base image

`nixos/harness-vm.nix` defines the initial x86_64 NixOS guest. The flake exposes
`nixosConfigurations.harness` and `packages.x86_64-linux.harness-image`.
It produces `harness.qcow2`, with its runtime Nix store inside the disk,
legacy BIOS/GRUB boot, an 8 GiB virtual disk, and diagnostics on serial port 0
at 115200 baud. Login is locked and DHCP, IPv6 and forwarding are disabled. A
noninteractive `harness` service account owns `/var/lib/harness`; a boot-time
readiness unit verifies its fixed UID/GID and state-directory access.
The two MAC-matched interfaces use fixed addresses with no default route or
DNS. The image contains the initial Rust `harness` binary; its boot-time
configuration unit uses that binary to validate the per-run manifest. The
model/tool loop has a deterministic library core tested with scripted fakes;
the VM does not run model requests yet.

The current application direction is a small Rust harness with a custom
model/tool loop. It will call the host's OpenAI-compatible Ollama endpoint,
expose `execute_target_command` and `submit` to the model, and send target
commands through the experiment broker. Inspect AI remains an option for a
later evaluation layer; it is not a planned runtime dependency for the first
end-to-end trial. See [summary.md](summary.md) for the broader architecture.

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
It can also be run directly as:

```sh
nix run .#harness -- check-config --manifest PATH
```

This expands the early version-1 schema; old two-field manifests are rejected.
Regenerate the configuration ISO when rebuilding the image. The endpoint
addresses, broker port, model, and limits above are example settings for a future
connected trial. The boot check only validates configuration and does not contact
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
Run-wide wall-clock/output limits and incremental event export are future
increments. The existing VM smoke test still exercises manifest loading.

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
VM is needed for these tests. Real Ollama interoperability remains untested.
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

## Target execution interface

`src/target/mod.rs` defines `TargetSession`, the experiment broker's interface
to a prepared container, VM, or other target. Trusted setup binds the target,
execution identity, shell, initial directory, environment, and limits. Each
`execute` request contains only a sequence number and command text. The broker
owns authentication and duplicate/out-of-order request rejection.

An `ExecutionReport` contains a sequence and an `ExecutionOutcome`. Only outcomes
where execution may have started carry raw stdout/stderr with truncation flags;
`NotStarted` cannot contain command output. Completed executions distinguish
parent-observed and guest-reported exit/signal status. Exit codes use `u8`, and
signals use `NonZeroU32` with additional platform validation at the adapter boundary.

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

This increment defines the trait, data types, and adapter obligations. Concrete
adapters and enforcement are still pending. The separate command protocol maps
these internal types to its wire schema.
The harness's `async_driver::CommandExecutor` remains the client-side interface;
the future broker will translate between HTTP and `TargetSession`.

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
The real broker and authentication remain pending.

## Run the harness

The executable supports running one trial from the validated manifest:

```sh
nix run .#harness -- run --manifest PATH > report.json
```

It creates a current-thread Tokio runtime, constructs both HTTP clients using the
manifest settings, and drives the existing async loop. Both endpoints must be
reachable from wherever the executable runs. No retries or automatic restart are
added. The intended deployment is the `harness` account inside the harness VM;
automatic systemd startup is a subsequent increment.

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
blindly. Run-wide history/report size limits and host-side collection are pending.

Run the executable integration tests locally with:

```sh
nix develop --command cargo test --locked --test cli_run
```

They create temporary manifests, launch the actual binary, and check requests
against fake inference/broker endpoints, stdout reports, stderr, and exit status.
They execute no target commands and require no VM or Ollama. The existing
`scripts/run-harness-smoke.sh` rebuilds and boots the package, but its disconnected
VM only runs `check-config`; a connected VM run test is still a later step.

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

Copy these files into the new `thorough-but-unreliable` repository. Merge the flake outputs
with that project's flake rather than replacing an existing application flake.
This is a configuration proposal, not a deployed container.
The Nixpkgs lock metadata is reused from the existing host repository. JSON,
and Nix syntax checks passed, and the image derivation evaluated against
the pinned revision. Image building and editor attachment still need validation.

## Applied changes

- Image reference: `localhost/thorough-but-unreliable-dev:latest`. Use the
  `latest` tag for now; rebuild, load and recreate after image changes.
- The development image includes the Rust compiler, Cargo, rustfmt, Clippy and
  rust-analyzer from the flake's pinned Nixpkgs revision.
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
- Add scoped cache volumes only when their benefit justifies persistence, then
  decide their retention and quotas.

No host configuration or running development environment is changed by saving
this proposal. Validate it with the chosen editor and rootless Podman before use.
