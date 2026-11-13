# Implementation status

Status is limited to behavior present in this checkout; the master plan's
release gates are not inferred from scaffolding or documentation.

| Area | State | Evidence / limitation |
|---|---|---|
| CI configuration | PARTIAL | `.github/workflows/ci.yml` defines Windows/Linux Rust and Python SDK jobs plus a PostgreSQL E2E job with a service container. The YAML parses; hosted CI has not run from this workspace, and the Linux commands were reproduced locally (see Verification). |
| Rust workspace and CLI shell | PARTIAL | Formatting, `cargo check --workspace --all-targets`, and `cargo test --workspace` pass on the temporary stable 1.99.1 toolchain used for this implementation. |
| `init` | PARTIAL | Creates project/workspace JSON and model/test/state directories without overwriting existing workspace config; remaining layout/bootstrap work is pending. |
| `validate` | PARTIAL | Validates project schema/version, model directory, static refs/cycles, source declarations, SQL template syntax, and the implemented config subset without requiring run-time variable values. |
| Automatic/manual `plan` | PARTIAL | Offline ordering and warnings; literal `ref()` / `env.depends_on()` only; no complete SQL/Python AST. CLI preflights SQL templates and relation resolution in planned order using ephemeral `--vars` and `def_var` values. |
| Safe SQL template renderer | PARTIAL | Rust API and CLI planning support bounded conditionals, `var()`/`def_var()`, static SQL macros, Jinja comments, strict/lenient undefined handling, and stable errors without value interpolation; SQL is discarded after validation. |
| Effective config | PARTIAL | Loads admin/user/workspace/project layers, reports field origins, and enforces documented administrative ceilings; workspace PostgreSQL profiles use environment variable references and are excluded from effective output. Full config catalog and secret providers are not implemented. |
| Sessions and variables | PARTIAL | Concurrent writes conflict explicitly (`DOWAI_SESSION_CONFLICT`, exit 3) under the default `error` policy; `wait_process` serialises runs of the same session through an OS file lock with a timeout.  Per-user local SQLite store supports UUID sessions, TTL expiry, typed JSON snapshots, bounded writes, and explicit end; values are plaintext, Unix permissions are restricted, and Windows ACLs are inherited. SQL and Python-only run paths persist variables only after all planned models succeed. |
| Python SDK | PARTIAL | Typed package with tests, plus `DowaiClient`, which runs the `dowai` binary with `--json` for plan, run, retry, runs, run detail and logs. The CLI and the client share one code path; 7 parity tests compare both on the same project and journal (AC034). A failed run that started reports its `run_id` on either path, the same identifier the journal stores (ENT-015). `Env` is not bound to the engine. |
| PostgreSQL adapter | PARTIAL | TLS-by-default adapter supports bound parameters, password environment-variable references, and transactional table/view replacement. SQL models are restricted to one statement, with quoted strings, dollar-quoted bodies, and nested comments handled by the guard; destructive relation changes require explicit adapter approval. Adapter smoke tests pass against PostgreSQL 17.4 with loopback TLS disabled; remote TLS is not verified. |
| SQL execution | PARTIAL | `run` plans, renders, resolves `ref()` and declared `source()` relations, and executes PostgreSQL table/view models with per-model atomic replacement; external writes are opt-in, replacing existing relations requires confirmation by default, sources are not plan nodes, and variables persist only after all selected models succeed. The opt-in runtime E2E verifies refs, sources, session persistence, and replacement approval against disposable PostgreSQL 17.4 with loopback TLS disabled. Full refresh (`--full-refresh --yes`) is implemented for PostgreSQL with staging and swap; schema drift fails by default. Other adapters are unsupported. |
| Python subprocess runtime | PARTIAL | Mixed SQL/Python runs (topological or manual order) share session variables with Python and SQL: SQL `def_var` is visible to later Python nodes and Python `setVar` selects SQL branches of later models; variables are published only if the whole run succeeds (E2E in `mixed_sql_python_e2e.rs`). Declared sources (`env.source`) also work in Python-only runs, with the PostgreSQL connection opened on the first read (ENT-111). Python-only plans run in a project-local venv through versioned bounded JSON IPC (NDJSON, one message per line), with a minimal inherited environment, timeout and process-tree termination on Windows (Job Objects) and Unix process groups (detached Unix children can escape), and `Env` variable/static-dependency operations. E2E covers successful ordering, variable persistence, model exception, unsupported operations/returns, malformed IPC, and timeout without committing staged values. Imperative `env.run("x.py")` from a Python model executes another Python model in a supervised child with inherited pending variables (merged back only on success), a recorded invocation tree (`RunResult.invocations`, `call_type` `static`/`imperative`/`dependency`), cycle and depth/invocation-count limits, the `env.manual()` context, and `DowaiExecutionError` with stable codes. `env.run("x.sql")` returns `DOWAI_NOT_IMPLEMENTED`. ENT-040: nested `env.run` returns an object with `run_id`, `invocation_id`, `status`, `failed`, `execution_mode` and `duration_ms` (the `run_id` is the parent run's); `env.plan(target, mode=None)` and `env.validate()` return the same JSON as `dowai plan --json` and `dowai validate --json` through the same engine functions (E2E `ac040_*`, and `env_plan_and_validate_match_the_cli_json_output` for CLI parity). `setVar`/`getVar` stay a local overlay, as before. Datasets: `env.ref(name).schema()` is implemented for SQL models of a mixed SQL+Python run (read-only host query with `prepare`, only relations declared in `depends_on`, 10 000 requests per run, stable `DOWAI_DATASET_*` codes; E2E `ac002_*`); `iter_rows(batch_size)` in batches of up to 10 000 rows, in physical order (`ORDER BY ctid`), exact `numeric` and typed scalars, refused for views with `DOWAI_DATASET_UNAVAILABLE` (E2E `ac002_iter_rows_*`); `select`, `filter` and `iter_batches` are implemented (ENT-041, E2E `ent041_*`): native-typed predicates are pushed to SQL, text-read equality falls back to the worker with a `DOWAI_DATASET_FILTER_NOT_PUSHED` warning, and lazy datasets are invalid outside their model; `to_*`, `env.source/write`, dependency installation/authorization, SDK binding, cancellation, persistent invocation journal, and sandboxing are not implemented. |
| Run journal | PARTIAL | Local SQLite journal (`runs.sqlite3`, schema version 2, owner-only permissions) records run and invocation status, timings, stable error codes, requested targets, profile, and variable count for every run outcome, including failures; `dowai runs list|show`, `dowai logs`, and `dowai retry` (B-05c) read it. Variable values, SQL, messages, and paths are not stored. No AUDIT stream. |
| Audit trail | PARTIAL | Separate append-only `audit.jsonl` (schema_version 1, owner-only permissions) receives run, node, variable-name, external-write, policy-denial and nested-invocation events with correlation IDs, UTC timestamps, engine version, and SHA-256 hashes of model, effective configuration, and plan. It is independent of log levels and cannot be disabled by project configuration. It is not immutable (no WORM or signing). Values, SQL, credentials, and absolute paths are never written. Not yet readable through the CLI. |
| Cancellation | PARTIAL | Ctrl+C/SIGTERM/SIGHUP (Unix) and Ctrl+C (Windows) cancel `dowai run` with status CANCELLED, exit 130 and `DOWAI_CANCELLED`. Python process trees are terminated; in-flight PostgreSQL statements are cancelled and rolled back; no variables are published. Tested with real SIGINT/SIGTERM on Unix and an in-flight `pg_sleep` against PostgreSQL 16.15. Not verified on Windows with a real signal. A commit completing before cancellation counts as succeeded. |
| Plugins / AI / OpenLineage | NOT_IMPLEMENTED | No execution path exists. |
| Windows/Linux installers | NOT_IMPLEMENTED | No distributable artifacts or release evidence. |

## Verification

Linux (Ubuntu 24.04, x86_64, rustc 1.99.1 stable, 2026-11-10):
`cargo fmt --all -- --check`, `cargo check --workspace --all-targets --locked`,
and `cargo test --workspace --locked` pass with 119 tests passed and 3 opt-in
PostgreSQL tests ignored. Building on Linux with the system OpenSSL headers
requires `OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu
OPENSSL_INCLUDE_DIR=/usr/include` when `pkg-config` is absent. The Python SDK
suite has 11 passing `unittest` cases (`PYTHONPATH=src`). The opt-in PostgreSQL
adapter smoke tests and runtime E2E pass against a PostgreSQL 16.15 cluster
running on loopback (ephemeral, user-owned, TLS disabled, scram-sha-256). This does
not verify remote TLS, production credentials, or PostgreSQL 17. Windows and
hosted CI runs have not been executed from this workspace. No installer has been
verified. Evidence log: `_gestao/EVIDENCIAS.md` (management folder, not tracked by Git).

The opt-in PostgreSQL tests require a dedicated disposable database and these
environment variables: `DOWAI_TEST_PG_HOST`, `DOWAI_TEST_PG_PORT`,
`DOWAI_TEST_PG_USER`, `DOWAI_TEST_PG_DATABASE`, and
`DOWAI_TEST_PG_PASSWORD`. Set `DOWAI_TEST_PG_TLS=disabled` only for a loopback
test server that has no TLS configured. Run them with:

```powershell
cargo test -p dowai-postgres --test smoke -- --ignored --test-threads=1
cargo test -p dowai-runtime --test postgres_e2e -- --ignored --test-threads=1
```
