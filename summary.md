# thorough-but-unreliable handoff

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
├── nixos/harness-vm.nix
├── scripts/run-harness-smoke.sh
└── src
    ├── lib.rs
    ├── manifest.rs
    ├── bin/harness.rs
    └── harness
        ├── types.rs
        ├── model_loop.rs
        ├── driver.rs
        ├── async_driver.rs
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

The only current executable operation is:

```text
harness check-config --manifest PATH
```

The typed manifest schema is deliberately limited to:

```json
{"version":1,"run_id":"smoke"}
```

It rejects missing, extra, or incorrectly typed fields, unsupported versions,
and empty run IDs. The model loop is not started by the executable yet.

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

One `submit` call finishes with its answer. Multiple submissions or a response
mixing submission and commands are protocol errors. A response without tool
calls is early termination. A model-turn budget bounds model calls while still
allowing every command in the last accepted response to finish.

Dependency failures stop immediately without retry and retain the completed
transcript. Command transport failures can be marked as completion-unknown.
Normal command outcomes, including nonzero exit, signal, and timeout, remain
data that can be shown to the model.

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
untested. Transport errors map to completion-unknown loop failures, caught
panics to `Panicked`, and other inference errors to `Failed` (failure to obtain
a usable response, without implying the server did no computation).

## Tests and validation

The suite currently has 32 passing tests. It includes property-based tests for:

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

The latest completed checks were:

```sh
nix develop --command cargo test --locked
nix develop --command cargo clippy --locked --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix build "path:$PWD#harness" --no-link
```

The original harness image passed its KVM smoke test after the Rust manifest
loader was installed. The image has not been rebuilt since the loop and
inference libraries were added; those libraries do not change its current
startup behavior.

## Harness VM status

`nixos/harness-vm.nix` builds an 8 GiB BIOS/GRUB QCOW2 with serial diagnostics,
locked login, no SSH, and a fixed noninteractive `harness` user (UID/GID 900).
Readiness units verify the account and the two MAC-matched interfaces. The
guest has no default route or DNS and disables IPv4/IPv6 forwarding.

A read-only ISO labeled `HARNESS_CONFIG` mounts at `/run/harness-config` with
`ro,nosuid,nodev,noexec`. When present, its `manifest.json` is validated by the
packaged Rust binary. Absence of the ISO remains valid for a base-image boot
test.

## Not implemented

- Experiment command HTTP client and its shared protocol types.
- `harness run`, expanded run manifest, system prompt/task loading, and a
  systemd evaluation service.
- Run-wide time/message/output limits and structured event export.
- Experiment broker, target adapter, experiment image, and evidence fixture.
- Physical-host controller, paired VM lifecycle, collection, and verifier.
- Connected inference/command networks and real Ollama interoperability.
- First end-to-end isolation trial.

## Next steps

### Immediate next increment

Define the narrow experiment command protocol and add its bounded HTTP client,
implementing the async driver's `CommandExecutor`. Discuss the protocol before
implementing it: authentication, request sequencing, timeout, output limits,
and uncertain completion. Test against a fake broker with no automatic retries.

### Subsequent increments

1. Run the full loop against fake inference and command endpoints.
2. Expand the manifest and add `harness run`; package and boot-test that service
   in the harness VM.
3. Implement one experiment broker/target adapter and its negative evidence
   control, then build the experiment image.
4. Connect both VM networks, verify the reachability matrix, and perform one
   command round trip.
5. Test real Ollama, add the minimum host controller/verifier, and run the first
   recorded end-to-end trial.
