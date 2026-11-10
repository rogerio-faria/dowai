# DOWAI

DOWAI (Data Objects With AI) is a Rust data-workflow engine. This repository
provides a project initializer, validator, static planner, local session store,
an initial PostgreSQL SQL-model runtime, and a limited supervised Python model
runtime.

## Current status

The planner discovers literal `ref("model")` / `ref('model')` and
`env.depends_on("model")` calls without executing SQL or Python, rejects
dynamic references and cycles, and supports automatic dependency ordering and
explicit manual ordering. Project and plan contracts use JSON schema version 2.

**Execution is partial.** PostgreSQL behavior has been exercised against an
isolated local PostgreSQL 17 disposable cluster with loopback TLS disabled;
remote TLS and production credentials have not been verified.
`dowai run` supports SQL-only models against a configured PostgreSQL profile,
using table/view materialization and static `ref()` dependencies. It atomically
replaces same-named relations in a per-model transaction. Python execution
currently supports Python-only plans, project virtual environments, versioned
JSON IPC, session-variable operations, and `None` model returns. Mixed
SQL/Python plans, Python datasets and dependency installation, MySQL,
cross-database writes, plugins, OpenLineage, AI, and release packages remain
unavailable. See
[docs/STATUS.md](docs/STATUS.md).

## Build

Requirements: a supported Rust toolchain with Cargo. A minimum supported
version is not declared until the compatibility matrix is established.

```powershell
cargo test --workspace
cargo build --workspace --release
```

## Quick start

```powershell
cargo run -p dowai-cli -- init exemplo --workspace .
cargo run -p dowai-cli -- validate .\projects\exemplo --json
cargo run -p dowai-cli -- plan faturamento.sql --project-dir .\projects\exemplo --json
```

Example model layout:

```text
projects/exemplo/
├── dowai_project.json
└── models/
    ├── stg_vendas.sql
    └── faturamento.sql
```

`faturamento.sql` may declare its static dependency as
`select * from {{ ref('stg_vendas') }}`. Python model dependencies may use
`env.depends_on("stg_vendas")`. Planning reads files as text; it never imports
or executes Python.

`plan --manual` preserves the supplied order and warns when a declared
predecessor is omitted or comes later. Automatic planning orders all static
predecessors first. `--vars` accepts a JSON object for template conditions and
validates SQL templates in plan order. `def_var` changes are temporary to that
planning operation; rendered SQL and variable changes are not persisted or
included in the plan output.

SQL templates support allowlisted conditionals, `ref()`, and
`config(materialized='table'|'view')`. Values are never interpolated into SQL.
Declare external PostgreSQL relations in the project `sources` map to resolve
static `source()` macros; source relations are not executable plan nodes. See
[configuration](docs/CONFIGURATION.md) for the JSON shape and restrictions.

## CLI currently available

- `dowai --version`
- `dowai init <name> [--workspace PATH]`
- `dowai validate [PROJECT_DIR] [--strict] [--json]`
- `dowai plan <model>... [--project-dir PATH] [--manual] [--vars JSON] [--json]`
- `dowai session start [--project-dir PATH] [--json]`
- `dowai session show <session_id> [--json]`
- `dowai session end <session_id> [--json]`
- `dowai config show --effective [--json]`
- `dowai doctor [--check-connections] [--json]`
- `dowai run <model|all> [--profile NAME] [--session ID] [--vars JSON] [--yes] [--json]`

`--json` keeps machine-readable output on stdout. Errors use stable
`DOWAI_*` codes and the exit codes documented in the master plan.
The implemented configuration layers and limits are documented in
[docs/CONFIGURATION.md](docs/CONFIGURATION.md).

`doctor --check-connections` uses `execution.default_profile` and the named
profile in the ancestor `workspace.json`. PostgreSQL profiles require `type`,
`host`, `port`, `user`, `database`, and a `password_env` variable name; TLS is
required by default. `tls: "disabled"` is permitted only for loopback hosts.
Profile secrets are never stored in JSON.
Database writes are denied unless effective `security.allow_external_writes`
is enabled. Replacing an existing table or view requires `--yes` by default;
administrator policy can require approval even if project configuration tries
to disable it.

Sessions use a local SQLite database under the platform's DOWAI state
directory, expire after 24 hours, and persist typed JSON variables through the
core API. The CLI's `session show` reports variable names but never values.
The initial limits are 1,000 variables, 64 KiB per value, and 16 MiB per
session; these limits and the TTL are not configurable yet. Session values are
stored as plaintext JSON in a per-user database, not encrypted; on Windows,
filesystem access follows inherited ACLs. Do not put credentials there.

To run SQL models, define a PostgreSQL profile in the ancestor `workspace.json`
and set `execution.default_profile` (or pass `--profile`). Profiles contain a
`password_env` reference, never a password. Enable
`security.allow_external_writes` in project configuration before running a
model; replacing an existing relation also requires `--yes` by default. For
example:

```powershell
# Add "config": {"security": {"allow_external_writes": true}} to
# dowai_project.json before executing models.
dowai plan faturamento.sql --vars '{"full_load":true}'
dowai run faturamento.sql --vars '{"full_load":true}' --yes --json
```

By default, the model's file stem is its PostgreSQL relation name. A SQL run
replaces an existing table/view of that name atomically for each model, but
previously completed models are not rolled back if a later model fails.
`source()` resolves only project-declared PostgreSQL relations using the same
connection profile as models. `--full-refresh` remains unsupported. Remote database
connections and TLS remain unverified as noted in
[docs/STATUS.md](docs/STATUS.md).

## Python SDK

A preliminary typed API package is in [src/dowai-sdk](src/dowai-sdk).
It is not connected to the Rust runtime yet and will fail clearly if runtime
operations are attempted without a backend.

### Initial Python model runtime

Create a project virtual environment and set `python.interpreter` in
`dowai_project.json` to its absolute interpreter path (`.venv\Scripts\python.exe`
on Windows or `.venv/bin/python` on Linux/macOS). A Python-only `run` does not
need a PostgreSQL profile, but effective `security.allow_external_writes` must
be enabled because model code is arbitrary trusted project code.

The current worker supports only `def model(env)`, `env.setVar()`,
`env.getVar()`, `env.unsetVar()`, static `env.depends_on()` declarations, a
`None` return, and `env.run("other.py")` for Python models (with
`env.manual()` and `mode="manual"`). Nested calls inherit pending variables,
merge them back only on success, and are limited by
`execution.max_call_depth` and `execution.max_invocations_per_run`; `env.run()`
of `.sql` files is not implemented yet. Variables commit to the session only
after every selected model succeeds. The worker runs in a supervised subprocess with a configured timeout
and bounded JSON IPC; Windows Job Objects and Unix process groups are
terminated on timeout. The worker receives a minimal environment allowlist,
not arbitrary host variables. This is **not a security sandbox**. Python code
can access the operating system, and models must be trusted; detached Unix
subprocesses can escape their process group. `env.ref()`, `env.source()`,
`env.run()`, datasets, non-`None` results, mixed SQL/Python plans, and package
installation fail explicitly as unsupported.

## Development

See [docs/ROADMAP.md](docs/ROADMAP.md), [docs/STATUS.md](docs/STATUS.md), and
the normative [plano_mestre.md](plano_mestre.md).
