# Changelog

## Unreleased

- Report the `run_id` of a run that started and then failed, in the `--json`
  envelope of `dowai run` and `dowai retry` and in the Python SDK result. The field
  is optional and absent when no run started (ENT-015).
- Add an initial Rust workspace with project initialization, validation,
  offline model planning, manual-order diagnostics, and versioned JSON output.
- Resolve implemented configuration layers in order with field origins and
  administrator ceilings; initialize workspace configuration without
  overwriting existing files.
- Add a safe offline SQL template renderer for bounded conditionals and
  session-variable defaults; SQL value interpolation remains prohibited.
- Add the preliminary typed Python SDK package; runtime integration is not
  included.
- Add imperative `env.run()` for Python models in the supervised runtime:
  nested invocations inherit pending session variables, merge them back only on
  success, record an invocation tree, enforce cycle, depth, and invocation-count
  limits, and raise `DowaiExecutionError` with stable codes. The worker IPC is now
  bidirectional NDJSON (`protocol_version` 1, additive); `env.run()` of `.sql`
  returns `DOWAI_NOT_IMPLEMENTED`.
- Add the GitHub Actions workflow (`.github/workflows/ci.yml`) referenced by the
  status document.
- Run mixed SQL and Python models in one plan: nodes execute in planned order,
  SQL `def_var` values reach later Python models, Python `setVar` values reach
  later SQL templates, and variables are published only after the whole run
  succeeds. Requires a configured PostgreSQL profile for mixed plans.
- Record every run outcome in a local run journal and add `dowai runs list` and
  `dowai runs show`. Failed runs keep their invocation history.
- Add `dowai logs <run_id>` over the run journal with status-based level filters.
- Add tests showing that CLI variables select SQL template branches without
  interpolating values (AC036), including a PostgreSQL case.
- Add a separate audit trail (`audit.jsonl`) with run, node, variable-name,
  external-write, policy-denial, and nested-invocation events, correlation IDs,
  and SHA-256 hashes. It is independent of log levels and not disableable per project.
- Cancel `dowai run` with Ctrl+C or SIGTERM (exit 130, status CANCELLED,
  `DOWAI_CANCELLED`). Python process trees and in-flight PostgreSQL statements
  are cancelled, and no variables are published.
- Detect concurrent writes to the same session variable: a run whose base revision
  changed fails with `DOWAI_SESSION_CONFLICT` and publishes nothing (no
  last-writer-wins). Session revisions are monotonic per session; the session
  store moves to schema version 1 with an automatic, idempotent migration.
- Detect concurrent writes to session variables explicitly (AC040). New
  `sessions.conflict_policy` (`error` default, or `wait_process`) and
  `sessions.wait_process_timeout_seconds` (default 300) serialise runs of the same
  session; a wait that times out fails with `DOWAI_SESSION_CONFLICT`.
