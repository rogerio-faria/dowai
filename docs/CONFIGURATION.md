# Configuration (implemented subset)

Native configuration files are JSON with `schema_version: 2`. The current
runtime consumes these sections: `execution`, `templates`, `security`, and
`ai`. Unknown sections and settings, malformed types, and unsupported schema
versions fail explicitly; this build does not claim support for the full
configuration catalog in the master plan.

## Layers and precedence

For settings without an administrator ceiling, values merge in this order
(later layers override earlier fields):

1. Engine defaults
2. Administrator config
3. User config
4. `workspace.json` (a nested `defaults` object is also accepted)
5. Project `config` object in `dowai_project.json`

Objects merge recursively. Arrays replace rather than merge. `null` means
inherit the previous value. The administrator layer can impose non-overridable
ceilings on external writes, destructive-operation approval, manual mode,
call/invocation limits, template nesting, and allowed path/host/profile lists.

On Windows, user and administrator paths follow `%APPDATA%\DOWAI\config.json`
and `%PROGRAMDATA%\DOWAI\config.json`. On Linux they follow
`${XDG_CONFIG_HOME:-~/.config}/dowai/config.json` and `/etc/dowai/config.json`.
Missing optional files are ignored; an existing unreadable or invalid file is
an error. Workspace config is found by walking from the project directory to
the nearest ancestor containing `workspace.json`.

The project layer is the `config` object in `dowai_project.json`. Workspace
defaults are stored in `workspace.json` under `defaults`; direct supported
configuration sections in that file override matching default fields. User
and administrator files contain supported sections at the root beside
`schema_version`.

Workspace files may also contain a `profiles` object for connection metadata.
The implemented adapter currently accepts PostgreSQL profiles in this form:

```json
{
  "schema_version": 2,
  "workspace_name": "analytics",
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
  "defaults": {
    "execution": {
      "default_profile": "warehouse"
    }
  }
}
```

`password_env` is a variable name, not a password value. Profile objects
reject unknown fields, require TLS unless explicitly disabled for a loopback
host, and are not included in effective configuration output. `doctor
--check-connections` uses `execution.default_profile`; the check performs only
a read-only `SELECT 1`. The SQL `run` path uses that profile to execute
PostgreSQL table/view models. Effective `security.allow_external_writes` must
be enabled before a run may write to PostgreSQL. Replacing an existing table
or view is separately subject to `security.require_approval_for_destructive`;
when approval is required, pass `--yes` to confirm replacement. Administrator
ceilings cannot be weakened by project or user settings.

Projects can declare external PostgreSQL relations for static `source()`
macros in `dowai_project.json`. The source and table names are template lookup
names; schema and relation names are quoted as PostgreSQL identifiers:

```json
{
  "schema_version": 2,
  "name": "sales",
  "models_dir": "models",
  "sources": {
    "warehouse": {
      "schema": "landing zone",
      "tables": {
        "orders": "order facts"
      }
    }
  }
}
```

`{{ source('warehouse', 'orders') }}` resolves to
`"landing zone"."order facts"` on the selected PostgreSQL connection. Sources
are external inputs, not executable plan nodes, and currently use the same
connection profile selected for model execution. Undeclared sources, dynamic
arguments, and unsupported adapters fail explicitly. Other profile types,
profile layering, full-refresh, and live database verification are not
implemented.

Python-only model execution is configured separately from database profiles:

```json
{
  "schema_version": 2,
  "name": "sales",
  "python": {
    "interpreter": "C:\\path\\to\\project\\.venv\\Scripts\\python.exe",
    "timeout_seconds": 300,
    "dependencies": [],
    "package_policy": "declared_and_authorized",
    "allow_unlisted_packages": false
  }
}
```

`python.interpreter` must point to an interpreter inside the project virtual
environment and the project directory; it is not selected implicitly from
`PATH`. Python-only runs do not use a PostgreSQL profile, but are still gated by
`security.allow_external_writes` because project code can perform arbitrary
side effects. The first worker version supports JSON session variables,
literal `env.depends_on()` and `None` returns. Declaring Python dependencies or
enabling unlisted packages currently fails explicitly because package
installation/authorization is not implemented. Python execution is not a
sandbox; execute only trusted project code. The worker inherits only a small
allowlist of runtime variables, not arbitrary host environment variables.
Unix models can detach subprocesses from the supervised process group, so
timeout termination is not a security boundary. Mixed SQL/Python plans and
dataset, `ref`, `source`, `run`, and write APIs are unsupported.
Non-default `python.package_policy` settings are also rejected until package
authorization is implemented.

Inspect the effective subset and per-field source with:

```powershell
dowai config show --effective --json
dowai config show --effective --project-dir .\projects\sales --json
```

The output contains `effective`, `origins`, and `sources`. Only present
configuration fields are returned; this command does not reveal connection
profiles or secrets.
