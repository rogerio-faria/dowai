# DOWAI

**Build data pipelines with SQL and Python in one dependency graph, running against PostgreSQL.**
You write the models. DOWAI works out the execution order, writes safely, and records every run.

- **One graph for SQL and Python.** `ref()`, `source()` and `env.depends_on()` define the order.
  A Python model can read a SQL model's output, and a SQL model can hand its result to Python with `run()`.
- **Safe by default.** External writes are off until you enable them. Replacing an existing table requires an explicit `--yes`.
- **Shared session variables.** `setVar` and `getVar` pass values between models. They are published only when the whole run succeeds.
- **Incremental loads.** A model declares its own watermark, which advances only after the write is committed.
- **Recoverable runs.** Transient PostgreSQL errors are retried with backoff. `dowai retry` reruns only what failed.
- **Auditable.** A local journal records each run and invocation with stable `DOWAI_*` error codes. Logs keep metadata and hashes, never variable values or SQL text.

## Quick start

Requirements: a Rust toolchain to build, a PostgreSQL database, and Python 3 with `venv` for Python models.

```bash
cargo build --workspace
export PATH="$PWD/target/debug:$PATH"
```

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
DOWAI writes into tables that already exist; it does not create them.

```bash
python3 -m venv .venv
export DOWAI_PG_PASSWORD='your-password'
```

```sql
create table regional_drop_alerts (
  region text not null, month date not null,
  amount numeric(12,2) not null, trailing_average numeric(12,2) not null
);
create table api_users (
  id int primary key, name text not null, email text not null,
  city text not null, company text not null
);
```

## Examples

### 1. SQL: a staging table from a source

`models/stg_orders.sql`

```sql
{{ config(materialized='table') }}
select id, customer_id, region, amount, sold_on
from {{ source('landing', 'orders') }}
where status = 'paid'
```

`source()` points to a table declared once in `dowai_project.json`.

### 2. SQL: aggregate another model

`models/customer_totals.sql`

```sql
{{ config(materialized='table') }}
select customer_id, count(*) as orders, sum(amount) as total_amount
from {{ ref('stg_orders') }}
group by customer_id
```

`ref()` sets the order: DOWAI runs `stg_orders` first, without you listing it.

### 3. SQL that calls Python: flag sudden regional drops

`models/sales_by_region.sql`

```sql
{{ config(materialized='table') }}
select region,
       date_trunc('month', sold_on)::date as month,
       sum(amount) as amount
from {{ ref('stg_orders') }}
group by region, date_trunc('month', sold_on)::date
{{ run("flag_regional_drops.py") }}
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

## Status and limitations

- SQL models run on PostgreSQL (tables and views). MySQL works as a write destination and as a Python source, not as the SQL execution profile.
- DOWAI writes into existing tables and does not create them.
- Re-running a table model fails while a view depends on that table, because the table is dropped and recreated. Drop the dependent view first.
- The test suite passes on Linux against PostgreSQL 16.15 and MySQL 8.0.46. Windows and hosted CI are not verified yet. See [docs/STATUS.md](docs/STATUS.md) for what is implemented, partial, or pending, and [docs/ROADMAP.md](docs/ROADMAP.md) for the plan.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
```

On Linux without `pkg-config`, set first: `OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include`.

## Documentation

- [Configuration](docs/CONFIGURATION.md), [CLI and API](docs/API.md), [compatibility](docs/COMPATIBILITY.md), [operations](docs/OPERATIONS.md), [security](docs/SECURITY.md)
- [Contributing](CONTRIBUTING.md) and [security policy](SECURITY.md)
