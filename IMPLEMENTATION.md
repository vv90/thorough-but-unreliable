# Implementation guidelines

- Use the type system to make impossible states unrepresentable. Prefer enums
  with variant-specific data over independent fields that permit contradictions.
  Use validated types for constrained values and derive redundant state instead
  of storing it separately. Validate external data when constructing these types.
- Use property-based tests where they fit. Test semantic invariants that hold
  across all valid states, or across an explicitly defined set of states with
  stated preconditions. Prefer these properties over tests that merely repeat
  the implementation.
- Put as much logic as possible into pure functions with explicit inputs and
  outputs, so properties can be tested efficiently and deterministically.
- Keep IO, networking, RNG, and other effects in a thin layer with a clear
  boundary around the pure logic. Pass effect results into the pure core.
- Never panic or use a panic as an escape hatch. Use explicit error handling
  and propagate meaningful errors. Avoid panic-prone shortcuts such as
  `unwrap()`, `expect()`, unchecked indexing, and unchecked arithmetic.
- Review external libraries and functions for possible panics. Prefer fallible
  APIs and validate their preconditions. Where a dependency can still panic,
  contain it at the dependency boundary and convert it into an explicit error
  when unwinding is supported. Panic catching cannot recover from process
  aborts; avoid APIs whose failure behavior cannot be safely handled.
