# DOWAI

DOWAI (Data Objects With AI) is a Rust data-workflow engine: SQL and Python
models run in one dependency graph, against PostgreSQL, with explicit policies
for every write.

## Why DOWAI

- **SQL and Python in one run.** A Python model declares `env.depends_on("stg_vendas")`
  and reads the rows of a SQL model with `env.ref("stg_vendas").iter_rows(...)`.
  The SQL model runs first, in the same session.
- **Writes are explicit and gated.** External writes are off by default
  (`security.allow_external_writes`). Replacing a relation needs `--yes`.
- **Incremental loads with a watermark the model declares.** The checkpoint is
  published only after the run succeeds.
- **Inspectable runs.** Each run has a local journal, stable `DOWAI_*` error
  codes, and an append-only audit trail that stores hashes, not values.

## Current status

**Execution is partial.** The full list of implemented behavior, limits and
evidence is in [docs/STATUS.md](docs/STATUS.md). In short:

- Planning, validation, SQL templates and the project/workspace configuration
  work offline.
- PostgreSQL SQL models (tables and views) run with per-model atomic
  replacement, `--full-refresh --yes` (staging and swap), and retries of
  transient failures.
- Python models run in a project virtual environment, supervised, with session
  variables, `env.ref()`/`env.source()` reads, `env.write()`, and nested
  `env.run()` of `.py` and `.sql` models.
- MySQL is supported as an `env.write()` destination and as an `env.source()`
  origin. It is not a SQL-model execution profile.

Plugins run through `dowai plugins run`. The official `files` plugin (CSV, JSON
Lines and Parquet) is available and tested. The `sftp` plugin is available as
code only: it has not been run against a real server. Not available yet: AI
features, OpenLineage, installers and release packages. Remote PostgreSQL TLS, Windows real-signal cancellation and
production credentials have not been verified.

## Build

Requirements: a supported Rust toolchain with Cargo. A minimum supported
version is not declared yet.

```bash
cargo build --workspace
cargo test --workspace
```

On Linux without `pkg-config`, set the OpenSSL location first:
`OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include`.

## Quick start

This takes about five minutes and needs a PostgreSQL role and database that you
can write to. Use a disposable local database for trying it out; the commands
below assume `dowai_demo` for both names.

```bash
# 0. Create the role and database (run as a PostgreSQL administrator).
createuser --pwprompt dowai_demo      # or create it with your own tooling
createdb --owner dowai_demo dowai_demo

# 1. Build the CLI (from this repository) and create a workspace elsewhere.
cargo build --workspace                       # run inside the DOWAI repository
export DOWAI="$(pwd)/target/debug/dowai"      # path to the binary you just built
mkdir -p ~/dowai-demo && cd ~/dowai-demo
"$DOWAI" init demo --workspace .              # creates ./projects/demo
cd projects/demo
```

Replace `workspace.json` (in the workspace root, the directory that contains
`projects/`) with a PostgreSQL profile. The password is read from the environment variable
named in `password_env`; it is never stored in the file:

```json
{
  "schema_version": 2,
  "workspace_name": "workspace",
  "profiles": {
    "warehouse": {
      "type": "postgres",
      "host": "127.0.0.1",
      "port": 5432,
      "user": "dowai_demo",
      "database": "dowai_demo",
      "password_env": "DOWAI_DEMO_PASSWORD",
      "tls": "disabled"
    }
  },
  "defaults": { "execution": { "default_profile": "warehouse" } }
}
```

`tls: "disabled"` is accepted only for loopback hosts. Use `"required"` for any
other host.

Create a virtual environment for Python models, then point the project at it
and allow external writes (the project `config` layer):

```bash
python3 -m venv --without-pip .venv
cat > dowai_project.json <<EOF
{
  "schema_version": 2,
  "name": "demo",
  "models_dir": "models",
  "python": { "interpreter": "$(pwd)/.venv/bin/python", "timeout_seconds": 60 },
  "config": { "security": { "allow_external_writes": true } }
}
EOF
```

The interpreter must be inside the project directory. The models below use only
the standard library, so the virtual environment needs no packages.

Two models: a SQL model, and a Python model that reads it with `env.ref()`:

```bash
cat > models/stg_vendas.sql <<'EOF'
select 1 as id, 'caneta' as produto, 10.5::numeric as valor
union all
select 2, 'caderno', 20.0::numeric
EOF

cat > models/faturamento.py <<'EOF'
def model(env):
    env.depends_on("stg_vendas")
    rows = list(env.ref("stg_vendas").iter_rows(batch_size=100))
    env.setVar("linhas", len(rows))
    env.setVar("total", sum(float(row["valor"]) for row in rows))
    return None
EOF
```

Validate, then run. `--yes` confirms replacing relations that already exist
from an earlier run:

```bash
export DOWAI_DEMO_PASSWORD='your-password'   # the password of the dowai_demo role
"$DOWAI" validate . --json
"$DOWAI" run all --yes --json
```

A successful run reports `"status": "SUCCEEDED"`. The variables `linhas` and
`total` are stored in the session. `dowai session show <session_id>` lists their
names; values are never printed by the CLI.

## Advanced examples

The incremental and full-refresh examples were run end to end against a local
PostgreSQL 16 database. The others are covered by the E2E tests in
`src/dowai-runtime/tests/` (`write_pg_e2e`, `full_refresh_e2e`,
`python_e2e`, `connections_e2e`, `retry_e2e`).

### Incremental load with a watermark

The model declares its own watermark. The session keeps it between runs, and it
moves forward only after the write is committed. Create the destination table
first; `env.write` appends to an existing table.

```sql
CREATE TABLE pedidos_demo (id integer, updated_at integer, status text);
```

```python
SOURCE = [
    {"id": 1, "updated_at": 10, "status": "ok"},
    {"id": 2, "updated_at": 20, "status": "ok"},
]

def model(env):
    watermark = env.getVar("pedidos_watermark", 0)
    delta = [row for row in SOURCE if row["updated_at"] > watermark]
    if delta:
        env.write(delta, target="warehouse", table="pedidos_demo", mode="append")
        env.setVar("pedidos_watermark", max(row["updated_at"] for row in delta))
    env.setVar("ultimo_delta", len(delta))
    return None
```

Run it twice in the same session (`dowai run pedidos.py --yes --session <id>`).
The first run loads both rows; the second loads none. `env.write` also supports
`replace` and `upsert` (with `keys`). Upsert needs a unique index on the keys.

### Full refresh through staging and swap

```bash
dowai run stg_vendas --full-refresh --yes --json
```

The new relation is built in a staging table and swapped in one transaction.
A schema change in the destination fails the run by default and keeps the
existing relation.

### Refuse direct library writes

Enable the policy in the project `config` layer:

```json
"config": { "security": { "deny_direct_library_writes": true } }
```

With it enabled, a Python model that imports a database driver or ORM to write
data (for example `import sqlalchemy`) is refused before it runs, with
`DOWAI_DIRECT_WRITE_DETECTED`. Without the policy, the same finding is a warning.

### Connection handles without secrets

A model asks for a connection by name and receives an opaque handle with only
`name` and `kind`:

```python
def model(env):
    conn = env.connections.get("approved")
    env.setVar("conexao", conn.name)
    return None
```

The handle has no URL, host, user or password attribute, cannot be assigned to,
and cannot be pickled. Only profiles allowed by `security.allowed_profiles` can
be requested.

### Retry failed runs

```bash
dowai retry <run_id> --failed-only --yes --json
```

`retry` starts a new run from a FAILED or CANCELLED run. Variables are not
journaled, so pass `--vars` again if the original run used any. Transient
PostgreSQL failures of SQL models are retried inside a run with backoff and
jitter; other failures, and Python models, are not retried.

## CLI

- `dowai init <name> [--workspace PATH]`
- `dowai validate [PROJECT_DIR] [--strict] [--json]`
- `dowai plan <model>... [--project-dir PATH] [--manual] [--vars JSON] [--json]`
- `dowai run <model|all> [--profile NAME] [--session ID] [--vars JSON] [--full-refresh] [--yes] [--json]`
- `dowai retry <run_id> [--failed-only] [--vars JSON] [--yes] [--json]`
- `dowai runs list|show`, `dowai logs <run_id>`
- `dowai plugins list|info|sync|resolve|run` (`run` needs `--idempotency-key` when the manifest requires it)
- `dowai session start|show|end`
- `dowai config show --effective [--json]`
- `dowai doctor [--check-connections] [--json]`

Every command prints machine-readable JSON with `--json`, and errors use stable
`DOWAI_*` codes. The configuration layers are described in
[docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Limitations

- **Python is not a sandbox.** Models are trusted project code. They can reach
  the operating system. The worker receives a minimal environment, and runs
  under a timeout that terminates its process tree.
- **Python packages are not installed by DOWAI.** Declaring Python dependencies
  or enabling unlisted packages fails explicitly.
- **`env.ref()` reads only SQL models of the same run.** Tables that are not
  produced by a model are read with `env.source()`, declared in
  `dowai_project.json`.
- **Dataset reads** return rows in physical PostgreSQL order (`ctid`).
- **MySQL** is a write destination and a source for `env.source()`, with
  `select` and batches ordered by primary key. Filters on MySQL sources are not
  supported yet, and MySQL tables cannot be SQL models. A transfer from
  PostgreSQL to MySQL is two independent transactions, not one atomic unit.
- **TLS** is verified only against local loopback servers with TLS disabled.
- The `sftp` plugin has no test run against an SFTP server, and its
  requirements are not yet hash-locked. AI features, OpenLineage and installers
  are not implemented.

Details and evidence for every item: [docs/STATUS.md](docs/STATUS.md). The
roadmap is in [docs/ROADMAP.md](docs/ROADMAP.md).

## Documentation and contributing

- [Configuration](docs/CONFIGURATION.md), [API](docs/API.md),
  [compatibility](docs/COMPATIBILITY.md), [operations](docs/OPERATIONS.md),
  [security](docs/SECURITY.md)
- [Contributing](CONTRIBUTING.md) and [security policy](SECURITY.md)
- The normative requirements are in [plano_mestre.md](plano_mestre.md).
