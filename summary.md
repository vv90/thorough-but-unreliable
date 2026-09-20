# thorough-but-unreliable handoff

## Latest increment (2026-09-20)

The live experiment is now configured by `nixos/experiment-service.nix`, which
imports the shared `nixos/experiment-vm.nix` foundation. The flake exposes
`experiment-image`. Target contents, Podman policy, and command settings are in
`targets/podman/smoke.nix` and selected through `targetEnvironment`.

`experiment-broker --config PATH` is a standalone Rust binary backed by
`src/broker/service.rs`: bounded strict configuration, validated adapter and
broker settings, explicit listener binding, signal-driven draining, and terminal
diagnostics. A separate systemd target service prepares the container and dynamic
configuration. It stops when the broker exits, removing the container before
systemd waits on preparation helpers, and checks the recorded cgroup is gone.
No automatic restart. The VM command NIC uses MAC `52:54:00:99:02:02` and
`10.99.2.2/30`, with TCP 8080 allowed on command0, no default route or forwarding.

`checks.x86_64-linux.experiment-service` boots the deployed service with an
isolated NIC and a guest test, checking real execution, deadline response,
broker exit, and systemd cleanup. `tests/broker_process.rs` covers executable
SIGTERM handling and terminal-response delivery without Podman. README documents
the current commands and bounds. This supersedes older statements below that
broker deployment is unimplemented. The user has also confirmed the older
real-Podman KVM smoke test passed.

Validation for this increment: the full Rust suite, Clippy, Rust/Nix formatting,
generated-shell ShellCheck, and Nix package build passed. A local TCG boot of
the final experiment smoke image passed execution, deadline delivery, successful
target-unit cleanup, and clean VM poweroff. Logs are in
`.artifacts/experiment-tcg.78QhGI/console.log`. This caught a conmon-related
cleanup delay, resolved by the separate target unit, and a test race, resolved
by awaiting successful unit shutdown before checking container/cgroup absence.
The committed sandboxed KVM check still needs to run on the user's KVM host.

Next: pair the harness and experiment command NICs in a sandboxed check, then
add the explicit host Ollama connection. Authentication remains deferred;
external evidence collection and isolation verdicts are still future work.

## Project direction

Build a small, reproducible framework for evaluating Nix-defined isolation
environments. A model proposes commands, but commands run only inside a
designated target through a narrow broker API. The physical host owns lifecycle,
evidence, collection, and verdicts.

This is early-stage work. The architecture below is the current direction and
may change as the first end-to-end trial exposes real constraints.

Use Rust for the application and a custom model/tool loop. Inspect AI may be
considered later as an evaluation or reporting layer; it is not a current
runtime dependency.

## Working rules

Follow `AGENTS.md` and `IMPLEMENTATION.md`:

- Work in small increments and discuss each next increment before applying it.
- Keep the state-transition logic pure and effects behind thin boundaries.
- Make impossible states unrepresentable using enums, validated types, and
  derived state rather than redundant independent fields.
- Use property-based tests for semantic invariants where they fit.
- Return explicit errors. Production code must not use panic, `unwrap`,
  `expect`, unchecked indexing, or similar escape hatches.
- Treat dependency panics as boundary failures where unwinding permits this;
  process aborts cannot be caught.

Cargo denies the corresponding Clippy lints.

## Intended deployment

```text
physical NixOS host
├── lifecycle controller, event collector, and evidence verifier
├── Ollama on host loopback
├── inference gateway at 10.99.1.1:11434
├── harness VM
│   ├── custom Rust model/tool loop
│   ├── inference client on inference0: 10.99.1.2/24
│   └── broker client on command0: 10.99.2.1/30
└── experiment VM or physical machine
    ├── command broker at 10.99.2.2/30
    ├── parent-only evidence
    └── designated isolation target
```

The inference and command networks have no default route, DNS, NAT, or
forwarding. The experiment is not connected to inference. The harness never
directly enters or manages the target and never receives the expected evidence
nonce. The host decides the verdict from externally retained evidence.

The first backend should use two disposable VMs. Later experiment backends may
use a physical machine without changing the harness command interface.

## Current repository structure

```text
.
├── Cargo.toml / Cargo.lock
├── IMPLEMENTATION.md
├── flake.nix / flake.lock
├── nixos/{harness-vm.nix,harness-connected-smoke.nix,podman-runtime-smoke.nix}
├── scripts/{run-harness-smoke.sh,run-harness-connected-smoke.sh}
├── tests/{broker.rs,podman.rs,podman_runtime.rs,podman_vm.rs,http_loop.rs,cli_run.rs,connected_vm.rs,support/mod.rs,fixtures/}
└── src
    ├── lib.rs
    ├── manifest.rs / manifest/{wire.rs,tests.rs}
    ├── command_protocol/{mod.rs,wire.rs,tests.rs}
    ├── broker/{mod.rs,http.rs,state.rs}
    ├── target/{mod.rs,tests.rs,podman/}
    ├── bin/harness.rs
    └── harness
        ├── types.rs
        ├── model_loop.rs
        ├── presentation.rs
        ├── report.rs / report/tests.rs
        ├── runner.rs
        ├── driver.rs
        ├── async_driver.rs
        ├── command/{mod.rs,tests.rs}
        ├── tests.rs
        └── inference
            ├── wire.rs
            ├── http.rs
            └── tests.rs
```

`README.md` is the source of truth for build and smoke-test commands. The shell
script is only a convenience wrapper and must remain synchronized with it.

## Implemented

### Harness package and manifest

The flake exposes `packages.x86_64-linux.harness` and
`packages.x86_64-linux.harness-image`. `rustPlatform.buildRustPackage` builds
the locked Cargo package. The pinned development shell and development OCI
image include Rust, Cargo, rustfmt, Clippy, and rust-analyzer.

The executable operations are:

```text
harness check-config --manifest PATH
harness run --manifest PATH
```

The version-1 schema now requires `run_id`, inline `system_prompt` and `task`,
`max_model_turns`, and nested `inference`/`command` settings. README contains the
complete example and field rules; the smoke script generates the same manifest.
The earlier two-field manifest is no longer accepted. Inference settings include
the completion URL, model, token limit, and HTTP limits; command settings include
the broker URL and HTTP limits. All settings are explicit, without defaults.

`Manifest` and nested settings have private fields and read-only accessors.
Validated positive integers use nonzero types. URLs follow the existing clients'
scheme/path/credential/query/fragment constraints. Timeouts are positive u32
milliseconds, converted to Duration without clock IO; connection timeout cannot
exceed total request timeout. Byte bounds fit isize::MAX. Prompt/task/model text
must be nonblank and is preserved exactly; run IDs retain the nonempty rule.

Pure `from_slice` and direct Serde deserialization validate schema and semantics;
reader/file methods provide IO. Nested nonobjects, duplicate/missing/unknown
fields, wrong types, and trailing JSON are rejected. Loader APIs bound the whole
manifest to 1 MiB and contain dependency unwinds; reader allocation is fallible.
Direct Serde callers must supply their own source-byte bound. Properties cover
preservation, numeric/text domains, timeout ordering, entry-point agreement, and
reader chunking. Configuration validation never constructs clients or contacts
endpoints. `check-config` retains its validation-only behavior.

### Executable run orchestration and final report

`src/harness/runner.rs` loads the manifest, creates a current-thread Tokio runtime,
constructs both clients from its settings, and drives the async loop. Tokio is
now a runtime dependency; process support is enabled only for tests. No retries
or automatic restart are added. `src/harness/report.rs` is a pure version-1 JSON
projection of the entire terminal RunReport, including run ID, outcome, and
ordered history. Target results retain lossless output, provenance, and usability;
typed failures retain payloads and completion uncertainty. README specifies the
report structure. The CLI emits JSON plus a newline to stdout and flushes it;
setup/output failures produce stderr diagnostics. Exit 0 requires submission and
successful report output; everything else exits 1. Submission is not a verifier
verdict. Output failure can leave a partial document. Final reports are not crash
recovery logs, and no run-wide output/history bound is implemented yet.

`tests/cli_run.rs` launches the actual binary with temporary manifests against
the same scripted endpoints as `tests/http_loop.rs` (shared support module).
It checks manifest-driven model/token settings, endpoints, request limits and
turn budget, complete report output, failure reports, exit status, and no extra
traffic. Child output/waits are bounded, and failed/timed-out children are killed
and reaped. Pure properties check report preservation and failure payloads;
writer tests inject errors/unwinds. The base VM still invokes only `check-config`.

### Deterministic loop core

`src/harness/model_loop.rs` is a pure, ownership-based state machine. It yields
exactly one next step:

- request a model response;
- execute one command; or
- return a finished report.

The caller consumes a continuation when supplying its result, so the same
effect cannot be completed twice. The core validates a complete assistant
response before allowing any command effect. It requires nonempty, unique call
IDs within each response, executes commands serially in response order, and
records each correlated result before requesting another model turn.
Dispatched commands and tool messages use validated `ToolCallId`; raw assistant
responses remain representable for diagnosis. Broker numbers use the distinct
`CommandSequence`, with checked increment. The command client must assign those
numbers and validate response correlation before returning a report.

One `submit` call finishes with its answer. Multiple submissions or a response
mixing submission and commands are protocol errors. A response without tool
calls is early termination. A model-turn budget bounds model calls while still
allowing commands in the last accepted response to finish while the target remains
usable.

Model failures and command-client failures have separate typed errors/outcomes.
Both stop immediately without retry. Tool messages retain complete target reports,
including raw partial output, truncation, completion source, and uncertainty.
Any unusable target report stops all later effects, even after a successful exit.
Recoverable rejection and confirmed deadline termination can continue when the
session is ready. Broker transport/decoding failures never masquerade as target
reports. `TargetSessionUnusable` points to the recorded report through its tool ID.

`presentation.rs` projects reports to model-facing JSON: output is UTF-8 text or
lossless hex with explicit encoding and truncation flags. Completion source,
error category, sequence, and readiness remain visible. The raw report stays in
history; this is not the broker wire schema.

`src/harness/driver.rs` is the current thin synchronous effect driver over
`ModelClient` and `CommandExecutor`. It catches unwinding dependency panics,
returns an explicit failure, and requires the adapters to be discarded. It
deliberately retains a foreign panic payload because dropping it could panic
again.

`src/harness/async_driver.rs` supplies async model/command interfaces and a
sequential `run(...).await` driver. `InferenceClient` implements the model
interface. It awaits each effect once, passing results into the same pure
state machine. Dependency unwinds during future construction or polling become
explicit failures. Dropping the run future produces no report and cannot undo
an already dispatched remote effect; cancellation/recovery is still future work.

### Inference HTTP client

`src/harness/inference/wire.rs` contains pure conversion between internal
messages and Ollama's OpenAI-compatible chat-completions JSON. Requests contain
both tools, `stream: false`, `n: 1`, and an explicit `max_tokens`.

The decoder requires one assistant choice at index zero and accepts only
supported finish reasons. It requires actual JSON objects, strict tool argument
schemas, nonempty unique call IDs, and supported tool names. Malformed,
ambiguous, unknown-tool, and token-truncated responses are errors.

`src/harness/inference/http.rs` is a thin asynchronous Reqwest client. Its
configuration supplies the full `/v1/chat/completions` URL, model, token limit,
connection and total-request timeouts, and request/response byte limits. It:

- checks the serialized request size before connecting;
- bounds response allocation while streaming the body, even without a truthful
  `Content-Length`;
- applies the total timeout through response-body reading;
- disables redirects, automatic retries, environment proxies, and HTTP/2;
- retains TLS verification through rustls; and
- treats dispatch and body-read failures conservatively as
  completion-unknown transport errors.

The inference client is connected to the async loop driver and tested with a
local fake HTTP server and in-memory command executor. Real Ollama remains
untested. Errors map to `ModelFailure` without erasing typed categories or
payloads (status, tool name, limits). Transport failures and panics imply possible
completion; other failures do not establish that the server did no computation.
The encoder rejects tool histories containing command-client failures or terminal
target reports.

### Target execution interface

`src/target/mod.rs` defines the experiment-side `TargetSession` async trait and
internal request/report types. Sessions bind one prepared target and its trusted
execution policy. Requests carry only a run-scoped protocol sequence and command.
Reports contain an `ExecutionOutcome`: only possibly-started outcomes have raw
partial output/truncation. Completed executions carry parent/guest provenance and
`u8` exit/runtime status codes or positive `NonZeroU32` signals (with platform
validation left to adapters). Runtime codes preserve uncertainty about exit versus
signal termination. Session usability is derived: unknown completion and uncertain deadlines
cannot declare readiness. Known completion/confirmed termination may still leave
a session unusable. Command rejection and session failure have separate types;
`NotStarted` cannot carry command output.

The contract requires serial execution without retries, fresh shells, EOF stdin,
bounded output and time, and explicit descendant policy. Uncertain completion or
protocol failure makes a session unusable; adapters must reject further requests.
Cancellation requires discarding the session and notifying supervision. An
uncertain deadline ends the trial through external supervision. Target lifecycle
and broker authentication/sequencing remain separate responsibilities.

The interface, data types, and pure readiness derivation are implemented. A
compiling adapter example and compile-fail examples exercise the public API.
The Podman adapter's pure core, validated setup, and Unix-socket transport are
implemented below, along with a real-runtime VM check. Production target lifecycle
management remains pending.
The existing harness-side `CommandExecutor` is a separate interface.

### Command protocol

`COMMAND_PROTOCOL.md` defines HTTP/1.1 `POST /v1/command`, strict JSON requests
and tagged execution reports, lossless lowercase hex output, and no retries.
Authentication is explicitly deferred. This protocol connects the harness to the
broker; guest channels remain independent behind `TargetSession`.

`src/command_protocol` implements pure codecs with bounded serialization and
body-size checks before parsing, preserving all target outcomes and rejecting
response sequence mismatches. A pure `SequenceTracker` starts at 1, reserves
one outstanding command, rejects unexpected requests without advancing, and
prevents reuse after terminal reports, mismatched reports, or abandonment.
Sequence exhaustion cannot wrap. HTTP IO/enforcement is provided by the broker
module. Property tests cover round trips, exact limits, correlation,
sequence transitions, and malformed inputs.

### Command HTTP client

`src/harness/command` implements the async driver's `CommandExecutor` using
the shared codecs and sequence tracker. `CommandConfig` supplies the full
HTTP(S) `/v1/command` endpoint, connection/total timeouts, and JSON body bounds.
Encoding borrows the command directly rather than cloning it before checking
limits. Reqwest uses HTTP/1.1, verified TLS, no redirects/retries/environment
proxies. Reports require HTTP 200 and exactly one unparameterized
`Content-Type: application/json` header (case-insensitive).

Body reads enforce limits independently of Content-Length, and the total
timeout includes response-body reading. A valid correlated report is returned
intact; only ready reports allow another sequence. Every client error ends the
session, including local oversized-request rejection. Sequence reservation before
network IO prevents reuse after cancellation of a pending future; external
supervision must still handle remote execution. No client clone/reset is exposed.
Dependency unwinds become typed failures where supported. Allocation and
unavailable-session failures retain conservative completion uncertainty.

Fake-broker tests cover ordered requests, recoverable and terminal reports,
exact byte limits, malformed responses, sequence mismatches, HTTP status/content
type, redirects, disconnects, chunked/EOF bodies, and timeouts/cancellation before
headers and during body reads. A property checks lossless bounded accumulation
and no buffer mutation on rejected chunks. Both HTTP clients also pass the full
loop integration tests described below. Authentication is still deferred.

## Tests and validation

The suite includes property-based tests for:

- command ordering and result/call-ID correlation;
- complete transcript preservation and deterministic replay;
- exact submission and early-termination behavior;
- validation before effects;
- model-turn budget enforcement;
- stopping without retry after dependency failure;
- lossless request and response wire conversion across generated Unicode text;
- rejection of malformed tool argument objects and unknown tools; and
- exact response-buffer limits, including leaving a buffer unchanged after a
  rejected append.

Target properties cover mandatory session discard after uncertain execution,
separation of reusable input rejection from terminal session failures, and
preservation of the adapter's readiness decision after known completion.
Compile-fail doctests reject readiness overrides on uncertain outcomes and
command output on a report whose command never started.
Harness properties cover all target-outcome branches, no effects after an unusable
report, exact retention of partial output and provenance, lossless presentation,
validated IDs, and nonwrapping command sequences. Manifest properties check every
parsing entry point; compile-fail examples block direct invalid construction and
mixing command sequences with tool IDs.

An additional generated-script property compares async and synchronous reports,
model inputs, ordered command calls, and unconsumed scripts across suspension,
turn budgets, malformed responses, and dependency failures.

Fake-server tests cover the complete inference request/response round trip,
malformed JSON, HTTP errors, redirects, disconnects, request/response limits,
chunked bodies, and timeouts both before headers and during body reads.
Async integration tests cover command batches, results in the next HTTP request,
submission, protocol rejection, turn limits, and stopping after inference or
command failures. Fault injection covers dependency unwinds during future
construction and after suspension.

`tests/http_loop.rs` uses both real HTTP clients and the public async driver with
two scripted localhost endpoints. One coordinator checks global request order,
request counts and payloads, and detects extra connections through completion
and a short quiet period afterward. Every scenario has a total timeout and the
server/run futures are joined without detached test tasks. Independent fixtures
specify broker JSON, internal reports, and model-facing results. Five scenarios
cover batch execution and submission; broker disconnect; unusable reports with
partial output; recoverable rejection; and inference failure after commands.
They check complete transcript preservation, binary output, truncation,
completion provenance, and no further traffic after terminal outcomes. No shell
commands execute. The Nix package source includes this integration test directory.

The latest completed checks were:

```sh
nix develop --command cargo test --locked
nix develop --command cargo clippy --locked --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix build .#harness --no-link
```

All 69 unit tests, 6 executable integration tests, 5 HTTP-loop integration tests,
1 local connected-fixture test, and 7 doctests passed. The explicit KVM test is
ignored by ordinary Cargo/Nix package checks. Clippy and the normal Git-backed
Nix harness package build passed. New source/test files are staged so the normal
flake includes them; no commit was made. Rust/Nix formatting, shell syntax and
ShellCheck pass. The connected image derivation and generated unit dependencies
evaluate successfully, but the VM has not been booted here (no /dev/kvm).

The user confirmed a successful disconnected KVM smoke test after `harness run`
was packaged. The new connected smoke test still needs its first external run.

## Harness VM status

`nixos/harness-vm.nix` builds an 8 GiB BIOS/GRUB QCOW2 with serial diagnostics,
locked login, no SSH, and a fixed noninteractive `harness` user (UID/GID 900).
Readiness units verify the account and the two MAC-matched interfaces. The
guest has no default route or DNS and disables IPv4/IPv6 forwarding.

A read-only ISO labeled `HARNESS_CONFIG` mounts at `/run/harness-config` with
`ro,nosuid,nodev,noexec`. When present, its `manifest.json` is validated by the
packaged Rust binary. Absence of the ISO remains valid for a base-image boot
test.

### Opt-in connected smoke image

`nixos/harness-connected-smoke.nix` extends the base image only for testing;
`packages.x86_64-linux.harness-connected-smoke-image` builds it. `harness-run`
requires/starts after account/network readiness, manifest checking and the ISO
mount. It runs as UID/GID 900, writes report.json under /var/lib/harness with mode
0600, refuses to overwrite an existing report, and never restarts automatically.
The test-only result unit verifies successful service completion, report ownership
and permissions, and complete JSON equality against the expected fixture, prints
PASS/FAIL, and powers off. The normal image keeps validation-only startup.

`tests/connected_vm.rs` reuses the scripted HTTP support to check three ordered
requests and reject missing/extra traffic. The KVM case creates an ISO and fresh
overlay under .artifacts, starts QEMU with separate restricted user networks and
explicit guestfwd mappings to loopback fixtures, and requires the guest PASS
marker and successful exit. A 180-second bound kills/reaps stalled QEMU; bounded
console/stderr logs and artifacts remain for inspection. The report stays inside
the overlay. No real target command runs. The ordinary local test checks the same
fixtures against the executable; the KVM case must be requested with --ignored.

README contains the authoritative commands. `scripts/run-harness-connected-smoke.sh`
invokes only sandboxed `nix build` for `checks.x86_64-linux.harness-connected-smoke`.
The check depends on the test image, requires KVM, and compiles/runs the Rust
test, QEMU, fake endpoints, and nc inside the Nix builder. No host devshell or
host Cargo execution is permitted; AGENTS.md records that boundary. The former
connected-smoke development shell has been removed. Successful checks publish
logs/ISO/overlay under result-harness-connected-smoke/artifacts; --keep-failed
retains the build directory on failure. Unchanged successful checks may be cached.
The new check derivation's KVM requirement, image dependency and test flags were
evaluated. A temporary verification override ran the local fixture instead of
QEMU and successfully installed its artifacts through the same Nix check recipe.
The user subsequently ran the full sandboxed KVM check on the host and reported
that it passed. It has not been rerun as part of the broker-library increment.
The fake wiring is not the future experiment network or an isolation-policy test.

### Experiment broker library

`src/broker` serves the command protocol over HTTP/1.1 using Hyper, with one
caller-supplied listener and prepared `TargetSession`. Private validated config
bounds request/response JSON bytes, connection capacity, and read/adapter/write
timeouts. One connection carries one request. Body limits apply to fixed-length
and chunked requests; header count/read buffer and total connection time are
bounded too. A capacity-one admission permit prevents queueing behind execution
or pending response delivery. The session owner applies `SequenceTracker` and
executes independently of the connection future; disconnect cannot cancel it.

Ready, correlated reports permit the next sequence after a successful local
response write. Invalid reports, delivery failure, unusability, or exhaustion
terminate the server. Adapter watchdog expiry/unwind creates an uncertain,
unusable report. Shutdown closes admission and drains work; callers must await
the server and supervise cleanup, since dropping it cannot establish termination.
Local TCP writes are not acknowledgments of client receipt. No retry or reset.

`tests/broker.rs` connects the real command client and raw TCP requests to the
real broker with controlled fake adapters. It covers sequencing, malformed and
oversized requests, concurrency, disconnect, shutdown, deadlines, connection
capacity, invalid reports, and adapter unwinds. Properties check body accumulation,
report bounds/correlation, and config validity. The Podman integration also uses
the broker with its real adapter against fake runtime endpoints. There is no
broker executable yet; the existing connected VM fixture is unchanged. Authentication remains
explicitly deferred. See README and COMMAND_PROTOCOL.md for the public contract.

### Podman adapter pure core

`src/target/podman/{config,session,wire,stream}.rs` implements validated setup,
bounded request/response codecs, an incremental non-TTY stream decoder, and a
pure session core. `transport.rs` implements IO separately as described below. Settings
bind a full container ID, local Unix socket, numeric user/group, absolute shell
and directory, explicit environment overrides, and command/JSON/output/deadline
limits. Environment overrides supplement the prepared container's base.

Owned `Create`, `Start`, `Capture`, and `Inspect` stages borrow one session
exclusively. Local invalid/oversized commands are rejected while ready; reserving
an attempt pessimistically poisons the session until successful completion.
Abandonment at any stage permanently prevents reuse. A complete stream and
correlated stopped exec/container inspection are both required for readiness.
Pending inspection can be repeated under the original deadline; start cannot.
The decoder retains bounded stdout/stderr prefixes, drains excess, and preserves
partial output on failure without allocating from advertised frame lengths.

The approved policy allows background descendants within a trial. Descendants
holding output streams open remain subject to the command deadline. Uncertain
execution ends the session and requires external trial teardown. The IO layer
enforces timers and HTTP bounds, validates upgrades, and notifies supervision
after cancellation/failure. The pure core only consumes observations.

`ProcessCompletion::RuntimeStatus` and wire kind `runtime_status` preserve numeric
runtime codes without inventing exit-versus-signal information. Codes remain u8;
137 does not prove SIGKILL. Codecs, model presentation, final report and generated
tests support this variant. Old version-1 decoders reject it, so update both ends
together. Fifteen focused tests include properties for exact command preservation,
chunk-independent bounded capture, cancellation/failure permanence, report
correlation, status fidelity, exact JSON bounds, and validated container selectors.

### Podman Unix-socket transport

`PodmanTargetSession` implements `TargetSession` over a caller-prepared binding
and a required Tokio oneshot supervision sender. It uses Hyper HTTP/1.1 over
UnixStream, one connection per request, with no detached connection tasks,
proxies, redirects, fallback endpoints, or automatic retries. A single deadline
covers all stages, including 25 ms pending-inspection delays and final decoding.
The broker watchdog must leave time for the adapter to report its own deadline.
HTTP parsing has 32-header/16 KiB buffer bounds; JSON bodies are bounded while
reading, including chunked/EOF bodies. Upgrade headers are validated and already
buffered raw stream bytes are retained. IO failures preserve captured output;
uncertain execution remains unusable. Dependency unwinds become explicit errors.

The supervisor receives one terminal event: SessionUnusable on a terminal report,
ExecutionCancelled if a polled execution future is dropped, or SessionDropped on
adapter disposal without an earlier event. No remote process termination is
claimed. A closed receiver prevents new commands. The caller must retain the
receiver and arrange cleanup on an event or unexpected channel closure. No target
lifecycle controller is added here.

Nine real Unix-socket tests in `tests/podman.rs` check ordered exchanges and no
retries, bounds, upgrades and prefetched frames, partial output, shared deadlines,
cancellation at every stage, supervision, and the full harness-client → broker →
adapter path. They use fake runtime endpoints, never actual Podman. A property
checks bounded JSON accumulation and unchanged buffers on rejection; fault
injection checks dependency unwinds and that expired operations are not polled.

### Real Podman VM check

`checks.x86_64-linux.podman-runtime` builds a standalone disposable experiment
image and runs it through a Rust KVM runner in the Nix build sandbox. README has
the authoritative command. No host Podman, development shell, network changes,
or shared directories are used. The VM has no NICs; a baked-in Nix container
archive supplies Bash and coreutils without pulls. Rootful Podman stays inside
the VM; the target runs as UID/GID 1000 with no socket mounts, no network, dropped
capabilities, no-new-privileges, read-only root and bounded resources.

`tests/podman_runtime.rs` is compiled as an opt-in test executable installed in
the guest image. It connects the real command client through a loopback broker
and adapter to Podman's Unix socket. Commands check identity/environment/workdir,
EOF stdin, binary output, nonzero runtime status, fresh shells with persistent
files, output truncation, and reuse. The last command leaves a child waiting:
the adapter must report an uncertain deadline with partial output. A fixture
supervisor consumes the terminal notice, removes the target, verifies absence
of container metadata and the target cgroup, and checks the broker's terminal
reason. A shell exit trap handles early failures; shutdown and the outer runner's
deadline provide fallback containment. This is test supervision, not a production
lifecycle controller.

`tests/podman_vm.rs` bounds QEMU runtime and log capture, kills/reaps on failure,
and checks the guest PASS marker and clean exit. Logs and the writable overlay
are installed in the check output. Both tests are ignored in ordinary Cargo
runs. Validation completed: full Cargo suite, Clippy, Rust/Nix formatting,
ShellCheck on the generated setup script, and the Nix harness package build.
There is no KVM device in the development workspace. A temporary local derivation
override allowed image assembly using Nixpkgs' QEMU TCG fallback, without changing
the committed KVM requirement. The resulting image booted under TCG and passed
all six command checks, supervision/cgroup cleanup and clean poweroff. That run
caught and fixed Podman's rejection of uid/gid tmpfs mount options; the fixture
uses mode 1777 for its isolated writable /work instead. Logs are in
`.artifacts/podman-tcg.DlHNZm/console.log`. The committed sandboxed KVM runner still
requires external acceptance. This fixture is not an isolation verdict or an
Ollama trial.

## Not implemented

- Command protocol authentication (explicitly deferred).
- Production run-service configuration and host report collection; the existing
  automatic startup and connected VM test are opt-in smoke fixtures only.
- Run-wide time/message/output limits and incremental structured event export.
- Broker executable/deployment, production experiment image, and evidence fixture.
- External KVM acceptance of the new real-Podman check.
- Physical-host controller, paired VM lifecycle, collection, and verifier.
- Connected inference/command networks and real Ollama interoperability.
- First end-to-end isolation trial.

## Next steps

### Immediate next increment

The user confirmed the sandboxed connected VM smoke test passed. The broker
library and complete Podman adapter are tested against fake runtime endpoints
and real Podman inside the disposable VM under TCG.
The real-Podman disposable VM check is now implemented. Run its sandboxed Nix
check on the KVM builder and resolve any runtime compatibility failures before
expanding the deployment. Then add the evidence control and broker deployment
as separately discussed increments.
Authentication remains a later increment. Keep development inside the devcontainer;
host build/test automation must execute through sandboxed `nix build` derivations.

### Subsequent increments

1. Verify the Podman adapter, add the negative evidence control and broker
   deployment, then build the experiment image.
2. Connect both VM networks, verify the reachability matrix, and perform one
   command round trip.
3. Test real Ollama, add the minimum host controller/verifier, and run the first
   recorded end-to-end trial.
