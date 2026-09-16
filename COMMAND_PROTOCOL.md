# Harness command protocol v1

The harness sends HTTP/1.1 `POST /v1/command` with `Content-Type` and `Accept`
set to `application/json`. Each request executes at most one command against the
broker's already prepared target. Responses are buffered, with no streaming or
automatic retries. Authentication is deferred to a later increment.
Responses require exactly one `Content-Type: application/json` header, with a
case-insensitive media type and no parameters; duplicate headers are rejected.

This is the harness-to-broker protocol. Container execution and nested-VM guest
channels remain behind `TargetSession` and need not use HTTP or this JSON schema.

## Request

```json
{"sequence":1,"command":"printf 'ok\\n'"}
```

Only these two fields are accepted. `sequence` is a JSON unsigned 64-bit integer;
`command` is a string preserved exactly, including whitespace and control
characters. Platform-specific rejection, such as a NUL that cannot appear in
shell argv, belongs to the adapter before dispatch. No target selector, identity,
environment, deadline, or resource-limit overrides are accepted.

## Execution report

HTTP `200` means the body is an execution report, including when execution failed.
The response echoes the request sequence:

```json
{
  "sequence": 1,
  "outcome": {
    "kind": "completed",
    "completion": {"kind":"exited","code":0,"source":"parent_observed"},
    "session_state": "ready",
    "output": {
      "stdout": {"hex":"6f6b0a","truncated":false},
      "stderr": {"hex":"","truncated":false}
    }
  }
}
```

Each outcome has exactly its variant's fields:

| `kind` | Additional fields |
| --- | --- |
| `completed` | `output`, `completion`, `session_state` |
| `not_started` | `failure` |
| `deadline_exceeded` | `output`, `execution_state` |
| `unknown` | `output`, `error` |

- `output` always contains separate `stdout` and `stderr` objects. Each has an
  even-length lowercase `hex` string and a required boolean `truncated` flag.
  Hex doubles raw output size. `truncated: false` does not prove a complete stream
  after execution or transport failure.
- `completion` is either `{"kind":"exited","code":0,"source":"parent_observed"}`
  or `{"kind":"signaled","signal":9,"source":"guest_reported"}`. Exit codes
  range from 0 to 255; signals range from 1 to 4294967295, with platform-specific
  validation left to adapters. Either completion kind allows either source.
- `session_state` is the string `ready` or `unusable`. A successful exit may
  still leave the session unusable. Guest-reported completion is not trusted
  evidence of containment or descendant termination.
- `execution_state` is `{"kind":"confirmed_stopped","session_state":"ready"}`
  (also permits `unusable`) or `{"kind":"may_still_be_running"}`. The latter
  cannot carry a readiness override and makes the session unusable.
- `failure` is `{"kind":"rejected","rejection":{"kind":"invalid_command","diagnostic":null}}`,
  `{"kind":"session_unusable"}`, or `{"kind":"failed","error":...}`.
  Rejection kinds are `invalid_command` and `command_too_large`; these are the
  only reusable not-started outcomes. An already unusable session cannot become
  ready through input rejection.
- `error` has a `kind` and optional string/null `diagnostic`. Kinds are
  `target_unavailable`, `transport`, `malformed_response`, `execution_mechanism`,
  `resource_exhausted`, and `dependency_panicked`. Rejection diagnostics use the
  same optional string/null representation. Diagnostics must omit parent secrets.

`unknown` is always unusable. Only possibly-started outcomes carry output.
The wire schema preserves the internal report without exposing its types as a
general Serde API. Model-facing presentation remains separate.

All object schemas reject unknown and duplicate fields, positional arrays,
missing required fields, invalid tags, and trailing JSON. Optional diagnostics
may be omitted. Numbers must be integer JSON values, not strings or fractions.

## Sequencing and failures

Each run has one client, one broker session, and one prepared target. The harness
assigns sequence numbers starting at 1. Both sides keep independent sequence
state. The broker reserves the expected number before dispatch and allows only
one outstanding command. Duplicate or out-of-order requests are rejected before
execution without advancing the counter. After a matching report, the next
number becomes available only if the session remains ready. Exhausting `u64`
ends the session's command capacity; the counter never wraps.

The client requires the echoed sequence to match its outstanding command. A
lost, malformed, oversized, or mismatched response ends the run; never retry the
command. Unusable sessions remain unusable. Cancellation discards the session
and requires supervision to handle any remote execution that may continue.
Sequence checks do not provide exactly-once execution across crashes or resets;
an existing run must not restart its sequence state.
The harness client also ends its local session after pre-dispatch errors such
as an oversized request, consistent with the loop's stop-on-client-error policy.
This differs from a valid broker report rejecting a command while remaining ready.

The future HTTP broker should use `400` for invalid JSON/schema, `413` for an
oversized body, and `409` for an unexpected sequence or outstanding command.
These are HTTP failures, not target reports. The client treats every status other
than `200` as failure to obtain a valid execution report and makes no assumption
that execution did not occur. It must not follow redirects or retry requests.

Trusted setup supplies request/response byte limits, execution deadline, capture
limits, and a longer bounded client timeout that allows cleanup and reporting.
The JSON limits include escaping, diagnostics, and hex expansion. The adapter's
capture limits must leave enough room for a complete response under that bound.
The broker enforces the execution deadline; the client timeout covers sending
and reading the whole response and cannot confirm termination.

## Current implementation

`src/command_protocol` provides pure bounded codecs, response correlation, and
`SequenceTracker`. Encoding bounds the output writer; decoding checks the full
body size before parsing. `src/harness/command` implements the async command
HTTP client, bounding body reads, enforcing status/content type/timeouts, and
invoking sequence transitions around IO. A pending request cancelled after
sequence reservation prevents reuse even though no report was obtained.
Local fake-broker tests cover transport failures and session reuse rules.
The broker and authentication remain unimplemented.
