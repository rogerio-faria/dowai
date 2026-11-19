# DOWAI

**Build data pipelines with SQL and Python across PostgreSQL, MySQL, Oracle and SQL Server.**
You write the models. DOWAI works out the execution order, writes safely, and records every run.

- **One graph for SQL and Python.** `ref()`, `source()` and `env.depends_on()` define the order.
  A Python model can read a SQL model's output, and a SQL model can hand its result to Python with `run()`.
- **Safe by default.** External writes are off until you enable them. Replacing an existing table requires an explicit `--yes`.
- **Shared session variables.** `setVar` and `getVar` pass values between models. They are published only when the whole run succeeds.
- **Incremental loads.** A model declares its own watermark, which advances only after the write is committed.
- **Recoverable runs.** Transient PostgreSQL errors are retried with backoff. `dowai retry` reruns only what failed.
- **Auditable.** A local journal records each run and invocation with stable `DOWAI_*` error codes. Logs keep metadata and hashes, never variable values or SQL text.

## Quick start

A project looks like this:

```text
sales/
├── dowai_project.json        # project, sources, Python interpreter, policies
├── workspace.json            # PostgreSQL profiles (passwords come from environment variables)
├── models/
│   ├── stg_orders.sql
│   ├── customer_totals.sql
│   ├── sales_by_region.sql
│   ├── flag_regional_drops.py
│   └── fetch_users.py
└── .venv/                    # Python environment used by the models
```

`workspace.json` declares the connection. The password is read from the variable named in `password_env`, never from the file:

```json
{
  "schema_version": 2,
  "profiles": {
    "warehouse": {
      "type": "postgres",
      "host": "localhost",
      "port": 5432,
      "user": "dowai",
      "database": "analytics",
      "password_env": "DOWAI_PG_PASSWORD",
      "tls": "required"
    }
  },
  "defaults": { "execution": { "default_profile": "warehouse" } }
}
```

`dowai_project.json` declares the source tables, the Python interpreter and the write policy:

```json
{
  "schema_version": 2,
  "name": "sales",
  "models_dir": "models",
  "python": { "interpreter": "/absolute/path/to/sales/.venv/bin/python", "timeout_seconds": 60 },
  "config": {
    "security": { "allow_external_writes": true }
  },
  "sources": {
    "landing": { "schema": "landing", "tables": { "orders": "orders" } }
  }
}
```

Prepare the environment once. The interpreter path must be absolute and inside the project.

```bash
python3 -m venv .venv
export DOWAI_PG_PASSWORD='your-password'
```


## Examples

### 1. SQL: a staging table from a source

`models/stg_orders.sql`

```sql
{{ config(materialized='table') }}

select 
    id, 
    customer_id, 
    region, 
    amount, 
    sold_on
from 
    {{ source('landing', 'orders') }}
where 
    status = 'paid'
```

`source()` points to a table declared once in `dowai_project.json`.

### 2. SQL: aggregate another model

`models/customer_totals.sql`

```sql
{{ config(materialized='table') }}

select 
    customer_id, 
    count(*) as orders, 
    sum(amount) as total_amount
from
    {{ ref('stg_orders') }}
group by 
    customer_id
```

`ref()` sets the order: DOWAI runs `stg_orders` first, without you listing it.

### 3. SQL that calls Python: flag sudden regional drops

`models/sales_by_region.sql`

```sql
{{ config(materialized='table') }}

select 
    region,
    date_trunc('month', sold_on)::date as month,
    sum(amount) as amount
from 
    {{ ref('stg_orders') }}
group by 
    region, 
    date_trunc('month', sold_on)::date

{{ run("flag_regional_drops.py") }}   -- calls a python file
```

`run()` goes at the end of the SQL file. After `sales_by_region` is materialized, DOWAI executes the Python model and passes the table to it as `origem`, a lazy dataset that is read in batches.

`models/flag_regional_drops.py`

```python
from collections import defaultdict

WINDOW = 3               # previous months used as the trailing average
DROP_THRESHOLD = 0.70    # flag a month that falls below 70% of that average


def model(env, origem=None):
    if origem is None:  # run directly (dowai run flag_regional_drops.py): nothing to read
        return None

    series = defaultdict(list)
    for batch in origem.iter_batches(500):
        for row in batch:
            series[row["region"]].append((row["month"], float(row["amount"])))

    alerts = []
    for region, points in series.items():
        points.sort()
        for i in range(WINDOW, len(points)):
            month, amount = points[i]
            history = [value for _, value in points[i - WINDOW:i]]
            average = sum(history) / len(history)
            if amount < DROP_THRESHOLD * average:
                alerts.append({
                    "region": region,
                    "month": month,
                    "amount": round(amount, 2),
                    "trailing_average": round(average, 2),
                })

    # The write is skipped when nothing was flagged: env.write rejects an empty batch.
    if alerts:
        env.write(alerts, target="warehouse", table="regional_drop_alerts",
                  mode="replace", confirm_replace=True)
    env.setVar("drop_alerts", len(alerts))
    return None
```

With sample orders where North's June sales fall to 40.00 against a trailing average of 106.67, the model writes one row to `regional_drop_alerts`: `north | 2026-06-01 | 40.00 | 106.67`.

### 4. Python first: load a REST API into a table

`models/fetch_users.py`

```python
import json
import urllib.request

USERS_URL = "https://jsonplaceholder.typicode.com/users"


def model(env):
    request = urllib.request.Request(USERS_URL, headers={"Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=15) as response:
        users = json.loads(response.read().decode("utf-8"))

    rows = [
        {
            "id": user["id"],
            "name": user["name"],
            "email": user["email"],
            "city": user["address"]["city"],
            "company": user["company"]["name"],
        }
        for user in users
    ]
    env.write(rows, target="warehouse", table="api_users",
              mode="replace", confirm_replace=True)
    env.setVar("users_loaded", len(rows))
    return None
```

This model uses only the standard library, so the project venv needs no packages. Run it on its own, without any SQL model:

```bash
dowai run fetch_users.py --json
```

## Running the project

Run these commands from the project directory:

```bash
dowai validate . --json                    # schema, references, cycles and sources
dowai plan all --json                      # execution order computed from the graph
dowai run all --yes --json                 # --yes allows replacing existing SQL tables
dowai runs list                            # local history of runs
dowai retry <run_id> --failed-only --yes   # rerun only what failed
```

Connection and policy details: [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Database support

| Database | SQL models | Python `env.source` / `env.write` |
| --- | --- | --- |
| PostgreSQL | Supported | Supported |
| MySQL | Supported | Supported |
| Oracle | Supported with limitations below | Supported with limitations below |
| SQL Server | Supported with limitations below | Supported with limitations below |

Oracle and SQL Server execution, source reads and writes have been tested against live
databases. The Oracle connector requires Oracle Client libraries.
Its `oracle_native` mode requires `TNS_ADMIN` to point
to a directory containing exactly this `sqlnet.ora`:

```text
SQLNET.ENCRYPTION_CLIENT = REQUIRED
SQLNET.ENCRYPTION_TYPES_CLIENT = (AES256, AES192, AES128)
SQLNET.CRYPTO_CHECKSUM_CLIENT = REQUIRED
SQLNET.CRYPTO_CHECKSUM_TYPES_CLIENT = (SHA512, SHA384, SHA256)
```

Oracle Client configuration is process-wide. Native encryption cannot be mixed with other
Oracle transport modes in one process, or enabled after Oracle Client was initialized elsewhere.

## Status and limitations

- SQL models can run on PostgreSQL, MySQL, Oracle or SQL Server (tables and views). A SQL model reads declared sources through the same selected profile and connection it uses for execution; a source declaring another profile is rejected when referenced. Nested SQL keeps that connection. Python can choose an origin with `env.source(name, table, profile=...)` and a destination with `env.write(...)`; transformations between databases are explicit user code, not automatic staging or type conversion. See [SAMPLES.md](SAMPLES.md) for examples.
- Oracle and SQL Server source tables require a primary key for paginated reads; filters are unsupported. Their non-null dataset values are returned as text, so Python must convert values as needed. `full_refresh` is unavailable on both; Oracle table replacement is not atomic.
- Oracle cancellation can be delayed by the server, and its per-round-trip timeout is not a hard total execution deadline. An interrupted SQL Server operation discards its connection; the outcome of an interrupted commit or DDL may be uncertain.
- DOWAI writes into existing tables and does not create them.
- Re-running a table model fails while a view depends on that table, because the table is dropped and recreated. Drop the dependent view first.
- PostgreSQL and MySQL have been tested on Linux; Oracle and SQL Server integration has been tested locally on Windows. Hosted CI has not verified these integrations. See [docs/STATUS.md](docs/STATUS.md) and [docs/ROADMAP.md](docs/ROADMAP.md).

## Build and test

```bash
cargo build --workspace
cargo test --workspace
```

On Linux without `pkg-config`, set first: `OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include`.

## Documentation

- [Database samples](SAMPLES.md)
- [Configuration](docs/CONFIGURATION.md), [CLI and API](docs/API.md), [compatibility](docs/COMPATIBILITY.md), [operations](docs/OPERATIONS.md), [security](docs/SECURITY.md)
- [Contributing](CONTRIBUTING.md) and [security policy](SECURITY.md)
