# Configuração

Arquivos de configuração são JSON com `schema_version: 2`. O catálogo completo de
parâmetros configuráveis (ENT-012, plano §3.4) está na seção **Catálogo de parâmetros**
abaixo. Chaves desconhecidas, tipos incorretos e valores fora do limite são recusados com
`DOWAI_CONFIG_INVALID`, citando o arquivo e a configuração.

Cada parâmetro tem um **estado**, que informa se ele já altera o comportamento:

- **em uso pelo runtime/CLI**: lido e aplicado hoje.
- **validado; ainda sem consumo pelo runtime**: o valor é validado, aparece em
  `dowai config show` e respeita tetos administrativos, mas não muda a execução até a
  tarefa de implementação correspondente.

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
PostgreSQL, MySQL, Oracle and SQL Server profiles use the same fields; select the database adapter
with `type`:

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
host, and are not included in effective configuration output. `doctor --check-connections` uses `execution.default_profile`; the check
performs only a read-only `SELECT 1`. SQL `run` uses the selected profile to
execute table/view models in PostgreSQL, MySQL, Oracle or SQL Server. Effective
`security.allow_external_writes` must be enabled before a run may write to a
database. Replacing an existing table or view is separately subject to
`security.require_approval_for_destructive`; when approval is required, pass
`--yes` to confirm replacement. Administrator ceilings cannot be weakened by
project or user settings.

Oracle profiles may select `tls: "oracle_native"` for required native AES encryption and
SHA-2 integrity, using the `TNS_ADMIN` configuration described in the README. This setting
is reserved for Oracle. Oracle and SQL Server profiles also support SQL execution,
`env.source()` and `env.write()`. Oracle's client timeout limits each database round trip,
not the total query duration; cancellation requested by the runtime can be delayed by
the server. An interrupted SQL Server connection is discarded, and the outcome of an
interrupted commit or DDL can be uncertain.

MySQL profiles typically use port 3306. All four adapter types are accepted for
`--profile`, `execution.default_profile`, `doctor --check-connections`,
`env.source()` and `env.write()`, under the same `security.allowed_profiles`,
`allowed_hosts`, TLS, and `allow_external_writes` policies. `env.write()`
supports `append`, `replace`, and `upsert`; MySQL upsert uses
`ON DUPLICATE KEY UPDATE` (and may be activated by any unique index on the
destination). Oracle and SQL Server paginated source reads require a primary key,
return non-null values as text, and do not support filters. SQL model datasets without
a primary key use `ROWID` on Oracle and binary-converted columns on SQL Server;
unsupported conversions or views without a primary key are rejected.

SQL models use one selected profile and connection for both source reads and
materialization. A SQL `source()` declaration that names a different profile
is rejected when the model references it; DOWAI does not stage or convert SQL
source data between databases. Nested `{{ run("other.sql") }}` calls continue
using that same profile. A SQL model may call Python with `{{ run("task.py") }}`;
that Python model can explicitly choose its own source and destination with
`env.source()` and `env.write()`.

MySQL DDL is not transactional. Existing table/view replacement is performed
through a staging relation and one `RENAME TABLE` statement; full refresh
requires the destination schema to match. A MySQL-to-PostgreSQL or
PostgreSQL-to-MySQL `env.write()` transfer uses independent transactions, so
there is no atomicity across databases.
Oracle DDL commits implicitly and table replacement is not atomic. `full_refresh`
is not implemented for Oracle or SQL Server. Transfers involving them likewise
are not atomic across databases.

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

`{{ source('warehouse', 'orders') }}` resolves using the selected SQL profile's
identifier quoting and database rules. Sources are external inputs, not
executable plan nodes. SQL models must use the same profile for the source and
execution; if a source declaration names a different profile, the model fails
when it references that source. For MySQL, `schema` must equal the selected
profile database and `tables` maps to tables in that database.

Python `env.source(name, table, profile=None)` reads declared sources from
either adapter (`schema()`, `iter_rows`, `select`). The optional `profile`
overrides the profile on the source declaration; if omitted, the declared
profile is used, falling back to the default execution profile. Profile and
host authorization policies still apply. MySQL tables need a primary key for
ordered batches, and `filter` on them returns
`DOWAI_DATASET_FILTER_UNSUPPORTED`. `env.ref()` remains limited to PostgreSQL
model datasets. Undeclared sources, dynamic arguments, and unsupported
adapters fail explicitly. Full refresh is supported for both adapters; MySQL
requires the destination schema to match.

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
`PATH`. Python-only runs do not use a PostgreSQL profile, except that
`env.run("x.sql")` uses `execution.default_profile` (ENT-114); they are still
gated by `security.allow_external_writes` because project code can perform
arbitrary side effects. The first worker version supports JSON session variables,
literal `env.depends_on()` and `None` returns. Declaring Python dependencies or
enabling unlisted packages currently fails explicitly because package
installation/authorization is not implemented. Python execution is not a
sandbox; execute only trusted project code. The worker inherits only a small
allowlist of runtime variables, not arbitrary host environment variables.
Unix models can detach subprocesses from the supervised process group, so
timeout termination is not a security boundary. `env.run` executes `.py` and
`.sql` models (ENT-114). A SQL model called this way runs with the caller's profile and
session, its `ref()` predecessors run first in automatic mode, and it is not added to the
static DAG. Dataset, `ref`, `source`, and write APIs are documented in their own sections.
Non-default `python.package_policy` settings are also rejected until package
authorization is implemented.

Inspect the effective configuration and per-field source with:

```powershell
dowai config show --effective --json
dowai config show --effective --project-dir .\projects\sales --json
```

The output contains `effective` (todos os parâmetros do catálogo, com default quando
não definidos), `origins` e `sources`. Este comando não revela perfis de conexão nem
segredos.

## Catálogo de parâmetros

Fonte única: `src/dowai-core/src/config_catalog.rs`. Defaults, validação e tetos
administrativos são derivados dele; `specs/config.schema.json` e este documento são
conferidos contra o catálogo pelos testes `tests/config_catalog.rs`.

Exceções intencionais, que **não** são camadas de configuração:

- `python.interpreter`, `python.timeout_seconds`, `python.package_policy` e
  `python.allow_unlisted_packages` pertencem ao bloco `python` de `dowai_project.json`,
  validado em `project.rs` (default de `timeout_seconds`: 300 s; `package_policy`:
  `declared_and_authorized`; `allow_unlisted_packages`: `false`). Não são aceitos dentro de
  `config`, para evitar duas fontes para o mesmo valor.
- `logging.audit_enabled` é **sempre** `true`: AUDIT essencial não pode ser desativado por
  projeto, usuário ou administrador.

Regras de teto administrativo (coluna "Teto administrativo"):

- **não pode exceder o teto**: valor efetivo limitado ao valor do administrador.
- **administrador `false` proíbe `true`** e **administrador `true` impõe `true`**: a
  configuração de projeto ou usuário não pode afrouxar a política.
- **interseção com a lista administrativa**: somente valores presentes na lista do
  administrador permanecem.

Unidades e limites aparecem na tabela. Limites mínimos e máximos são inclusivos.

### `runtime`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `runtime.max_parallel_nodes` | `4` | inteiro 1–256 | nós | não pode exceder o teto administrativo | aplicado pelo scheduler: no modo automático, nós independentes de uma onda executam em paralelo, com no máximo `min(max_parallel_nodes, max_worker_processes)` simultâneos (ENT-120, ADR 0008 §2); no modo manual, sequencial (concorrência efetiva 1). Aviso em `warnings` somente quando o scheduler é sequencial, há nós independentes e o limite é maior que 1 |
| `runtime.max_worker_processes` | `4` | inteiro 1–64 | processos | não pode exceder o teto administrativo | teto de processos simultâneos do scheduler: cada nó Python executa em seu próprio worker e cada nó SQL usa conexão própria (ADR 0008 §5); o limite efetivo de paralelismo é `min(max_parallel_nodes, max_worker_processes)` |
| `runtime.max_threads` | `8` | inteiro 1–256 | threads | não pode exceder o teto administrativo | `not_enforced` (ENT-104): não há limitação de threads; aviso em `warnings` em todo run com nós |
| `runtime.max_cpu_percent` | `100` | inteiro 1–100 | % de CPU | não pode exceder o teto administrativo | `not_enforced` em todas as plataformas nesta versão (D06); abaixo de 100 gera aviso e, com `require`, recusa o worker |
| `runtime.max_memory_mb` | `2048` | inteiro 64–1048576 | MiB | não pode exceder o teto administrativo | Linux: `RLIMIT_AS` por worker Python (memória **virtual**, estado `partial`); fora do Linux `not_enforced` até ENT-093. Bibliotecas que reservam muita memória virtual podem precisar de valor maior |
| `runtime.max_inflight_bytes` | `134217728` | inteiro 1–1099511627776 | bytes | não pode exceder o teto administrativo | `not_enforced` (ENT-104): o buffer de `env.ref`/`iter_batches` é por linhas (`batch_size`), não por bytes; aviso em `warnings` em todo run com nós Python |
| `runtime.queue_capacity` | `1000` | inteiro 1–1000000 | itens | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `runtime.shutdown_grace_seconds` | `10` | inteiro 0–600 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `execution`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `execution.default_mode` | `"automatic"` | enum: `automatic`, `manual` | — | — | em uso pelo runtime/CLI |
| `execution.default_profile` | `null` | texto não vazio ou `null` (herda) | nome do perfil | — | em uso pelo runtime/CLI |
| `execution.stop_on_error` | `true` | booleano | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `execution.allow_manual_mode` | `true` | booleano | — | administrador `false` proíbe `true` | em uso pelo runtime/CLI |
| `execution.max_call_depth` | `16` | inteiro 1–256 | níveis | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `execution.max_invocations_per_run` | `1000` | inteiro 1–100000 | invocações | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `execution.node_timeout_seconds` | `3600` | inteiro 1–86400 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `execution.run_timeout_seconds` | `86400` | inteiro 1–604800 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `execution.retry_max_attempts` | `1` | inteiro 1–10 | tentativas totais, incluindo a primeira | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `execution.retry_backoff_ms` | `1000` | inteiro 0–600000 | ms | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `execution.retry_jitter` | `true` | booleano | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `templates`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `templates.strict_undefined` | `true` | booleano | — | — | em uso pelo runtime/CLI |
| `templates.allow_dowai_if_alias` | `true` | booleano | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `templates.max_nesting_depth` | `32` | inteiro 1–64 | níveis | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `templates.max_render_bytes` | `1048576` | inteiro 1024–104857600 | bytes | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `templates.allowed_macros` | `[]` | lista vazia (não suportado nesta versão) | nomes; nesta versão só lista vazia (ADR 0004) | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `security`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `security.allow_external_writes` | `false` | booleano | — | administrador `false` proíbe `true` | em uso pelo runtime/CLI |
| `security.deny_direct_library_writes` | `false` | booleano | — | administrador `true` impõe `true` | em uso pelo runtime: recusa run com escrita direta por biblioteca detectada (ENT-049) |
| `security.require_approval_for_destructive` | `true` | booleano | — | administrador `true` impõe `true` | em uso pelo runtime/CLI |
| `security.allow_insecure_tls` | `false` | booleano | — | administrador `false` proíbe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `security.allowed_paths` | `[]` | lista de textos | caminhos | interseção com a lista administrativa | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `security.allowed_hosts` | `[]` | lista de textos | hosts | interseção com a lista administrativa | em uso pelo runtime/CLI |
| `security.allowed_profiles` | `[]` | lista de textos | perfis | interseção com a lista administrativa | em uso pelo runtime/CLI |

### `plugins`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `plugins.require_manifest` | `true` | booleano | — | administrador `true` impõe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `plugins.allow_local_override` | `true` | booleano | — | administrador `false` proíbe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `plugins.max_parallel_calls` | `4` | inteiro 1–64 | chamadas | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `plugins.default_timeout_seconds` | `300` | inteiro 1–86400 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `plugins.allow_network` | `false` | booleano | — | administrador `false` proíbe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `plugins.allow_filesystem` | `false` | booleano | — | administrador `false` proíbe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `plugins.allow_secrets` | `false` | booleano | — | administrador `false` proíbe `true` | validado; plugin que declara `secrets` só executa com `true` (ENT-056); plugin `rest` usa o segredo `rest_token` quando `token` não é informado (ENT-109) |
| `plugins.max_output_bytes` | `1048576` | inteiro 1024–67108864 | bytes | não pode exceder o teto administrativo | validado; limite do resultado JSON (stdout) do plugin (ENT-056) |
| `plugins.require_idempotency_key` | `true` | booleano | — | administrador `true` impõe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `logging`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `logging.level` | `"INFO"` | enum: `ERROR`, `WARNING`, `INFO`, `DEBUG` | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021); consumido pelo log operacional `logs/dowai.jsonl` (ENT-065) |
| `logging.audit_enabled` | `true` | booleano, sempre `true` | AUDIT essencial: sempre true | — | AUDIT essencial: sempre ativo; AUDIT não é afetado pelo log operacional |
| `logging.retention_days` | `90` | inteiro 1–3650 | dias | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021); consumido pelo log operacional `logs/dowai.jsonl` (ENT-065) |
| `logging.rotation_max_mb` | `64` | inteiro 1–10240 | MiB | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021); consumido pelo log operacional `logs/dowai.jsonl` (ENT-065) |
| `logging.compression` | `"gzip"` | enum: `none`, `gzip` | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021); consumido pelo log operacional `logs/dowai.jsonl` (ENT-065) |
| `logging.redaction_enabled` | `true` | booleano | — | administrador `true` impõe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021); consumido pelo log operacional `logs/dowai.jsonl` (ENT-065) |
| `logging.console_format` | `"text"` | enum: `text`, `json` | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `lineage`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `lineage.enabled` | `false` | booleano | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `lineage.emit_mode` | `"disabled"` | enum: `disabled`, `file` | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `lineage.output_path` | `"lineage/openlineage.jsonl"` | texto não vazio | caminho relativo ao projeto | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `lineage.namespace` | `"dowai"` | texto não vazio | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `lineage.fail_on_emit_error` | `false` | booleano | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `ai`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `ai.enabled` | `false` | booleano | — | administrador `false` proíbe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `ai.default_profile` | `null` | texto não vazio ou `null` (herda) | nome do perfil | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `ai.max_tokens` | `4096` | inteiro 1–1000000 | tokens | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `ai.max_cost_per_run` | `0.0` | número ≥ 0 | unidade monetária do provedor; 0 = nenhum custo permitido | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `ai.timeout_seconds` | `60` | inteiro 1–600 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `ai.allow_data_rows` | `false` | booleano | — | administrador `false` proíbe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `ai.redaction` | `true` | booleano | — | administrador `true` impõe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `ai.profiles` | `{}` | objeto | perfis de IA (objeto) | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `sessions`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `sessions.conflict_policy` | `"error"` | enum: `error`, `wait_process` | — | — | em uso pelo runtime/CLI |
| `sessions.wait_process_timeout_seconds` | `300` | inteiro 1–86400 | s | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `sessions.ttl_seconds` | `86400` | inteiro 60–31536000 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `sessions.max_variables` | `1000` | inteiro 1–100000 | variáveis | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `sessions.max_value_bytes` | `65536` | inteiro 1–10485760 | bytes | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `sessions.max_context_bytes` | `1048576` | inteiro 1–104857600 | bytes | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `sessions.allow_resume` | `true` | booleano | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `sessions.persist_state` | `true` | booleano | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

### `database`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `database.pool_max_connections` | `4` | inteiro 1–100 | conexões | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `database.connect_timeout_seconds` | `10` | inteiro 1–600 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `database.query_timeout_seconds` | `1800` | inteiro 1–86400 | s | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `database.fetch_batch_rows` | `10000` | inteiro 1–1000000 | linhas | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `database.write_batch_rows` | `10000` | inteiro 1–1000000 | linhas | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `database.max_transfer_bytes` | `1073741824` | inteiro 1–1099511627776 | bytes | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `database.tls_required` | `true` | booleano | — | administrador `true` impõe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

#### Destinos de `env.write` (ENT-127)

- `env.write(..., target="NOME")` aceita somente perfis declarados em `workspace.json` (`profiles`) e permitidos pela política (`security.allowed_profiles`). O nome não declarado é recusado com `DOWAI_CONFIG_INVALID` (mensagem: `Unknown connection profile 'NOME'`) **antes de qualquer conexão**.
- Tipos suportados: `postgres`, `mysql`, `oracle`, `sqlserver` e `external` (driver Python, ENT-128).
- Os modos são fixos: `append`, `replace` (com `confirm_replace=True`) e `upsert` (com `keys`). A escolha do método correto, a ausência de duplicidade e as cláusulas de filtro são responsabilidade do usuário; o DOWAI garante perfil, schema, política e transação no destino.
- `origem` pode ser `None` quando o modelo Python é executado diretamente; o arquivo deve tratar esse caso (ver `examples/sql-run-post`).

#### Perfil `external`: destino com driver Python (ENT-128)

Destinos sem adaptador embutido usam um driver Python escolhido por configuração, sem recompilar o DOWAI. Exemplo em `workspace.json`:

```json
"profiles": {
  "sqlserver_dw": {
    "type": "external",
    "driver": "sqlserver/driver.py",
    "params": { "schema": "dw", "batch_size": 500 },
    "env": ["SQLSERVER_TOKEN"]
  }
}
```

- `driver`: arquivo `.py` relativo a `<projeto>/drivers/`. Sem `..`, barra invertida, caminho absoluto ou link simbólico. Erro: `DOWAI_WRITE_DRIVER_INVALID`.
- `params`: objeto de parâmetros não secretos. Segredo literal é recusado com `DOWAI_CONFIG_INVALID`; use variável de ambiente.
- `env`: nomes de variáveis de ambiente que o driver recebe. Só os nomes ficam na configuração; a ausência de uma variável declarada é recusada antes de qualquer linha.

Interface do módulo (funções de nível de módulo; todas obrigatórias, ausência gera `DOWAI_WRITE_DRIVER_INVALID`):

| Função | Contrato |
|---|---|
| `open(params, table, columns, mode, keys, confirm_replace)` | Prepara uma sessão de escrita. |
| `write_batch(rows) -> int` | Grava um lote de mapeamentos com exatamente as colunas declaradas. Retorna a quantidade. |
| `commit() -> dict` | Torna a sessão durável e devolve os contadores em um dicionário compatível com JSON. |
| `abort()` | Descarta a sessão. |

O driver roda em processo supervisionado, com o interpretador do projeto em modo isolado. Ele deve desfazer a escrita se o processo encerrar sem `commit`, porque o runtime encerra o processo após qualquer falha. Erros do driver com código `DOWAI_*` são reportados com esse código; demais erros viram `DOWAI_WRITE_DRIVER_FAILED` com mensagem sanitizada. Saída do driver em `stdout` é desviada para `stderr`. Não há isolamento de rede nem de arquivos do driver (mesma limitação de §2 de `docs/SECURITY.md`).

### `python`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `python.dependency_lock_required` | `true` | booleano | — | administrador `true` impõe `true` | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `python.install_tool` | `"uv"` | enum: `uv` | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `python.venv_path` | `".venv"` | texto não vazio | caminho relativo ao projeto | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `python.max_threads` | `4` | inteiro 1–256 | threads | não pode exceder o teto administrativo | `not_enforced` (ENT-104): validado, sem uso pelo runtime; aviso em `warnings` em todo run com nós Python |
| `python.resource_enforcement` | `"best_effort"` | enum: `best_effort`, `require` | — | — | `best_effort`: limites reportados em `warnings`. `require`: worker com limite solicitado e `not_enforced` não inicia, com `DOWAI_RESOURCE_UNSUPPORTED` (ENT-038) |

### `state`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `state.backend` | `"sqlite"` | enum: `sqlite` | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `state.path` | `"state/dowai.sqlite"` | texto não vazio | caminho relativo ao projeto | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `state.busy_timeout_ms` | `5000` | inteiro 0–600000 | ms | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `state.checkpoint_policy` | `"after_commit"` | enum: `after_commit` | — | — | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |
| `state.retention_days` | `90` | inteiro 1–3650 | dias | não pode exceder o teto administrativo | validado; consumido por `render_template`, `render_sql_model` e preparação do run (ENT-021) |

## Segredos e origem dos valores (ENT-013)

- **Segredo literal não entra em nenhuma camada** (engine, administrador, usuário,
  workspace ou projeto). Chaves como `password`, `secret`, `token`, `api_key` e `apikey`
  com valor textual são recusadas com `DOWAI_CONFIG_INVALID`. A mensagem nomeia a chave
  (`ai.profiles.warehouse.api_key`), nunca o valor, e o erro sai como JSON em stdout.
- Use referências: `*_env` (nome de variável de ambiente) ou `*_ref` (gerenciador de
  segredos). Referências são aceitas e aparecem na saída como nome, não como valor.
- `dowai config show --effective --json` devolve `effective`, `origins` e `sources`. Como
  segredo literal é recusado antes da saída, não há valor a mascarar; testes sentinela
  (`tests/config_show.rs` e `dowai-cli/tests/config_show_cli_e2e.rs`) garantem que o valor
  não aparece em stdout, stderr nem na mensagem de erro.

Valores de `origins` por campo:

| Valor | Significado |
|---|---|
| `engine_default` | default do catálogo, nenhuma camada definiu o campo |
| `admin` | definido pelo arquivo do administrador |
| `user` | definido pelo arquivo do usuário |
| `workspace` | definido em `workspace.json` |
| `project` | definido em `config` de `dowai_project.json` |
| `admin_ceiling` | valor reduzido por teto administrativo |

Origens são registradas por folha (por exemplo, `ai.profiles.warehouse.api_key_env`).

## Legacy YAML (AC032)

Each scope uses one format. The JSON file is the only one read.

- A scope with both `<name>.json` and `<name>.yaml` (for example `dowai_project.json` and
  `dowai_project.yaml`) fails with `DOWAI_CONFIG_CONFLICT`. Layers are never merged.
- A scope with only the legacy YAML fails with `DOWAI_CONFIG_LEGACY_YAML` and points to the
  migration command. The YAML is not read as a layer.

Convert a legacy file with:

```
dowai config migrate <file.yaml> --output <file.json> [--json]
```

The command never modifies the input, never replaces an existing output, refuses literal
credentials (`password`, `secret`, `token`, `api_key`, `apikey` with a value; use `*_env`
references instead) without echoing them, and checks the written file as a native
configuration. Parser errors do not quote the input.
