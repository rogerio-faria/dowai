# Implementation status

Status is limited to behavior present in this checkout; the master plan's
release gates are not inferred from scaffolding or documentation.

| Area | State | Evidence / limitation |
|---|---|---|
| CI configuration | PARTIAL | Windows/Linux Rust and Python SDK jobs are defined in `.github/workflows/ci.yml`; hosted CI has not run in this workspace. |
| Rust workspace and CLI shell | PARTIAL | Formatting, `cargo check --workspace --all-targets`, and `cargo test --workspace` pass on the temporary stable 1.99.1 toolchain used for this implementation. |
| `init` | PARTIAL | Creates project/workspace JSON and model/test/state directories without overwriting existing workspace config; remaining layout/bootstrap work is pending. |
| `validate` | PARTIAL | Validates project schema/version, model directory, static refs/cycles, source declarations, SQL template syntax, and the implemented config subset without requiring run-time variable values. |
| Automatic/manual `plan` | PARTIAL | Offline ordering and warnings; literal `ref()` / `env.depends_on()` only; no complete SQL/Python AST. CLI preflights SQL templates and relation resolution in planned order using ephemeral `--vars` and `def_var` values. |
| Safe SQL template renderer | PARTIAL | Rust API and CLI planning support bounded conditionals, `var()`/`def_var()`, static SQL macros, Jinja comments, strict/lenient undefined handling, and stable errors without value interpolation; SQL is discarded after validation. |
| Effective config | PARTIAL | Loads admin/user/workspace/project layers, reports field origins, and enforces documented administrative ceilings; workspace PostgreSQL profiles use environment variable references and are excluded from effective output. Full config catalog and secret providers are not implemented. |
| Sessions and variables | PARTIAL | Per-user local SQLite store supports UUID sessions, TTL expiry, typed JSON snapshots, bounded writes, and explicit end; values are plaintext, Unix permissions are restricted, and Windows ACLs are inherited. SQL and Python-only run paths persist variables only after all planned models succeed. |
| Python SDK | PARTIAL | Typed preliminary package with tests; no Rust IPC/runtime integration. |
| PostgreSQL adapter | PARTIAL | TLS-by-default adapter supports bound parameters, password environment-variable references, and transactional table/view replacement. SQL models are restricted to one statement, with quoted strings, dollar-quoted bodies, and nested comments handled by the guard; destructive relation changes require explicit adapter approval. Adapter smoke tests pass against PostgreSQL 17.4 with loopback TLS disabled; remote TLS is not verified. |
| SQL execution | PARTIAL | `run` plans, renders, resolves `ref()` and declared `source()` relations, and executes PostgreSQL table/view models with per-model atomic replacement; external writes are opt-in, replacing existing relations requires confirmation by default, sources are not plan nodes, and variables persist only after all selected models succeed. The opt-in runtime E2E verifies refs, sources, session persistence, and replacement approval against disposable PostgreSQL 17.4 with loopback TLS disabled. Full-refresh and other adapters are unsupported. |
| Python subprocess runtime | PARTIAL | Python-only plans run in a project-local venv through versioned bounded JSON IPC, with a minimal inherited environment, timeout and process-tree termination on Windows (Job Objects) and Unix process groups (detached Unix children can escape), and `Env` variable/static-dependency operations. E2E covers successful ordering, variable persistence, model exception, unsupported operations/returns, malformed IPC, and timeout without committing staged values. Mixed SQL/Python, datasets, `env.ref/source/run/write`, dependency installation/authorization, SDK binding, cancellation, and sandboxing are not implemented. |
| Plugins / AI / OpenLineage / audit store | NOT_IMPLEMENTED | No execution path exists. |
| Windows/Linux installers | NOT_IMPLEMENTED | No distributable artifacts or release evidence. |

## Verification

Rust verification on Windows: `cargo fmt --all -- --check`, `cargo check
--workspace --all-targets --locked`, and `cargo test --workspace --locked`
pass (94 tests passed; 3 opt-in PostgreSQL tests ignored). The supervised
Python runtime E2E and CLI E2E pass with a local project virtual environment;
Linux and hosted CI validation remain pending. The Python SDK suite has 11
passing `unittest` cases. Opt-in PostgreSQL adapter and
runtime E2E tests pass against an isolated local PostgreSQL 17.4 cluster using
loopback trust authentication and TLS disabled; this does not verify remote
TLS or production credentials. No installer has been verified.

The opt-in PostgreSQL tests require a dedicated disposable database and these
environment variables: `DOWAI_TEST_PG_HOST`, `DOWAI_TEST_PG_PORT`,
`DOWAI_TEST_PG_USER`, `DOWAI_TEST_PG_DATABASE`, and
`DOWAI_TEST_PG_PASSWORD`. Set `DOWAI_TEST_PG_TLS=disabled` only for a loopback
test server that has no TLS configured. Run them with:

```powershell
cargo test -p dowai-postgres --test smoke -- --ignored --test-threads=1
cargo test -p dowai-runtime --test postgres_e2e -- --ignored --test-threads=1
```
