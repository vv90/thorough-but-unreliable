# Working direction

This is an early-stage project: treat existing notes and architecture as current
direction, not fixed requirements.

Work one small step at a time. Present directions in step-by-step format, giving
the user a chance to discuss each step before implementing it or moving on.
Avoid large changesets and long plans built on assumed decisions.

Keep development execution inside the devcontainer. Never require `nix develop`
or a development shell on the host. New host-side build/test automation must execute
through sandboxed `nix build` derivations, including connected VM test runners.

Read and follow [IMPLEMENTATION.md](IMPLEMENTATION.md) for implementation and
testing rules: pure logic, thin effect boundaries, property-based tests for
semantic invariants, and explicit error handling without panics.
