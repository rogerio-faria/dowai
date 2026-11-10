# ADR 0001: Offline planner boundary

- Status: Accepted; initial boundary extended by the partial M2 runtime
- Date: 2026-11-08

## Context

The workspace had no implementation or Rust toolchain available at the start
of this work. The product plan requires the engine to be Rust and forbids
success-shaped mocks for unfinished execution paths.

## Decision

The initial usable boundary was project initialization, validation, and
side-effect-free SQL/Python static planning. The later M2 slice adds an
Application/runtime API for SQL-only PostgreSQL `table`/`view` execution with
static `ref()` resolution and real transactional DDL; it does not simulate
database success. Unsupported paths, including Python and `source()`, remain
explicit errors. Keep the Python package independent and unbound rather than
creating a second execution engine.

## Consequences

- This remains a partial M1/M2 deliverable, not a complete DOWAI release.
- Static references must be literal `ref("name")` calls. Other dynamic
  constructs fail planning rather than being silently omitted.
- PostgreSQL live execution is not verified until an authorized E2E run has
  passed against a dedicated test database.
- Rust behavior must be compiled and tested with Cargo before being marked
  verified.
