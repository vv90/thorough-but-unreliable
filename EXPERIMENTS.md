# Setting up an experiment

An experiment combines a target environment with a trial manifest. The current
runner boots two disposable VMs: the harness calls the model and sends commands
to a broker in the experiment VM, which executes them in a Podman container.

Use this guide for authoring an experiment. The [README](README.md) is the source
of truth for build/run commands, host prerequisites, limits, and artifact paths.
Work on definitions inside the devcontainer; run host-side experiments through
the sandboxed Nix launcher. No host development shell is required.

## 1. Choose or define the target

First decide whether an existing target supplies the environment your task needs.
Multiple manifests can reuse one target; changing a prompt does not require a
new container definition.

| Target | Environment | Example trial |
| --- | --- | --- |
| `smoke` | Bash/coreutils, empty writable `/work` | [hello.json](trials/hello.json) |
| `config-repair` | Order-summary app, broken config, data, inspection tools, and an in-container check | [config-repair.json](trials/config-repair.json) |

For a new environment, create `targets/podman/NAME.nix`. Use
[smoke.nix](targets/podman/smoke.nix) for the basic contract or
[config-repair.nix](targets/podman/config-repair.nix) for an environment with
initial files and preparation. Each definition is a function accepting `pkgs`
from the pinned Nixpkgs, returning:

- `image`: the container archive with its tools, files, and image configuration.
- `imageReference`: the image name and `latest` tag matching that archive.
- `command`: broker UID/GID, absolute shell and working directory, and explicit
  environment overrides. These must be usable inside the chosen container.
- `runArgs`: individual Podman argument strings defining its runtime policy.
- Optional `prepareCommand`: an argv list executed inside the container, as its
  configured user, before the broker starts accepting commands.

Set `image.config.User`, `WorkingDir`, `Env`, and `Cmd` consistently with the
broker settings. The initial process must keep the container alive throughout
the trial; current targets use `sleep infinity`. Include all tools and data in
the image so setup does not require registry pulls or downloads at runtime.

Both current targets run as UID/GID 1000 with no network, a read-only root, no
capabilities, and bounded resources. Their `/work` is a writable 1 MiB tmpfs.
A tmpfs mount hides files baked into the image at that path: place pristine
files elsewhere in the image, then copy them into `/work` during preparation.
The config-repair target demonstrates this and gives the copied files writable
permissions. Do not try to change the ownership/mode of the root-owned tmpfs
mount itself as UID 1000.

Preparation runs after container/cgroup recording and before broker startup.
Its failure aborts setup and invokes cleanup. Workspace edits persist across
commands in one trial, but each command starts a fresh shell. Container teardown
removes the tmpfs contents. The adapter permits background processes within a
trial; a command result does not establish that all descendants have stopped.

## 2. Register a new target

Add its name to [targets/default.nix](targets/default.nix), for example:

```nix
my-task = import ./podman/my-task.nix { inherit pkgs; };
```

Target definitions are trusted deployment configuration: their runtime arguments
can change isolation policy. Selection happens in the launcher, not in the model's
tool calls or the manifest. The runner passes the selected definition into the
experiment VM; the harness image and command API do not need a task-specific edit.

Add new Nix definitions and referenced assets to Git before using the Git-backed
flake. A trial manifest supplied by filename may remain untracked or outside the
repository; the launcher copies it into the Nix store explicitly.

## 3. Write the trial manifest

Copy an existing file under [trials/](trials/) and edit:

- `run_id`: a nonempty descriptive identifier retained in the report.
- `system_prompt` and `task`: instructions and the concrete objective. Explain
  what files may change and which command can verify the work, if available.
- `max_model_turns`: the maximum number of model requests.
- `inference.model`, token budget, and request limits: settings for the available
  model. The current examples use `qwen3.5:9b-q4_K_M`.

Supply every field in the [manifest schema](README.md#manifest-validation).
Prompts are inline JSON strings; there is no file inclusion or environment
expansion. Keep these addresses for the current runner:

| Field | Required value |
| --- | --- |
| `inference.completion_url` | `http://10.99.1.1:11434/v1/chat/completions` |
| `command.command_url` | `http://10.99.2.2:8080/v1/command` |

These are guest addresses. The launcher connects inference to the host gateway
socket and commands to the experiment VM's private link. Other addresses are
rejected before VM startup. Keep the example command request timeout of 60000 ms
and response bound of 1048576 bytes unless deliberately changing the transport
configuration; the broker has its own execution and capture limits.

Choose a turn budget and inference timeout that fit the 720-second trial
deadline. More allowed turns do not extend that deadline. The complete manifest
and exported report are each limited to 1 MiB. Large transcripts may exceed the
export limit even when each individual request fits its limits.

The supplied file controls the model. The wrapper rejects `INFERENCE_MODEL` when
a manifest is supplied; `INFERENCE_SOCKET` independently selects the host gateway.
The launcher's fresh build ID prevents cached results from substituting for a new
run. It does not rewrite the manifest's `run_id`.

## 4. Validate the setup

Inside the devcontainer, validate a manifest without calling either endpoint:

```sh
nix develop --command cargo run --locked --bin harness -- \
  check-config --manifest trials/config-repair.json
```

For an environment with a known defect, add a deterministic Nix check when useful:
verify preparation, reproduce the initial failure, apply the intended repair,
and verify the expected result. The existing example is exposed through
`checks.x86_64-linux.config-repair`; see its
[verification command](README.md#configuration-repair-experiment). Such a check
proves the exercise setup, not the model's ability to solve it or VM isolation.

For a small model, keep the first task bounded: a few files, one defect, installed
tools, useful error messages, and roughly 6–8 turns. The config-repair example asks
the model to find a bad input path, edit the config, and run `check-orders`.

## 5. Run on the host

The host needs x86-64 KVM, Nix sandboxing with the `kvm` system feature, and the
inference gateway socket accessible to build users and exposed inside the sandbox.
Use the [inference probe](README.md#real-inference-tool-call-probe) to check that
connection when provisioning a host. Host gateway configuration is managed outside
this repository; the launcher does not create it.

From the repository root on the host:

```sh
bash scripts/run-harness-local.sh --target config-repair trials/config-repair.json
```

Replace the target and manifest with your registered name and file. `--target`
must come before the manifest. It defaults to `smoke`; a different target requires
a manifest. With no arguments, the launcher runs the strict real-inference smoke
test. The corresponding raw Nix command is in the
[README](README.md#configuration-repair-experiment).

The runner builds the images, creates fresh overlays and a configuration ISO,
waits for target/broker readiness, runs the harness, collects its report, and
requests experiment shutdown. It checks broker/target service shutdown and
container/cgroup removal. No automatic trial retry is performed.

## 6. Inspect the result

For a supplied manifest, `local trial: RECORDED` means a report was collected and
cleanup verified. It does not mean the model achieved the task. Inspect:

- `harness/report.json`: terminal outcome, conversation, commands, and results.
- `config/manifest.json`: the original supplied manifest.
- `harness/` and `experiment/` console/stderr logs: startup and shutdown diagnostics.
- `artifacts/target.json`: selected target, image store path/reference, runtime
  arguments, command settings, and preparation command.

Per-run files are below
`result-harness-local/artifacts/harness-connected.TIMESTAMP.SUFFIX/`;
`target.json` sits directly under `artifacts/`. Names contain UTC date/time and
sort by run type, then time. The output link points to the latest successful
build; a failed build leaves any earlier successful link in place. Inspect the
failed build directory printed by `--keep-failed` for its available artifacts.

Submission, turn-limit exhaustion, early termination, and model/command failures
are all recordable outcomes. Missing or invalid reports, supervisor timeouts,
guest crashes, and failed cleanup fail the build. Neither an in-container check
nor the model's submission is an external isolation verdict.

No automatic expectations or answer grader are implemented for supplied trials.
For config-repair, inspect whether the command result contains
`PASS: 3 orders, total 42` and whether the preceding commands made the requested
repair. The no-argument smoke test separately requires exact known results.

Only Podman targets inside the experiment VM are currently provisioned. Nested
VM and bare-metal variants need their own setup and adapters; they are not
selectable backends yet.
