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
| `runtime.max_parallel_nodes` | `4` | inteiro 1–256 | nós | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `runtime.max_worker_processes` | `4` | inteiro 1–64 | processos | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `runtime.max_threads` | `8` | inteiro 1–256 | threads | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `runtime.max_cpu_percent` | `100` | inteiro 1–100 | % de CPU | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `runtime.max_memory_mb` | `2048` | inteiro 64–1048576 | MiB | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `runtime.max_inflight_bytes` | `134217728` | inteiro 1–1099511627776 | bytes | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `runtime.queue_capacity` | `1000` | inteiro 1–1000000 | itens | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `runtime.shutdown_grace_seconds` | `10` | inteiro 0–600 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |

### `execution`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `execution.default_mode` | `"automatic"` | enum: `automatic`, `manual` | — | — | em uso pelo runtime/CLI |
| `execution.default_profile` | `null` | texto não vazio ou `null` (herda) | nome do perfil | — | em uso pelo runtime/CLI |
| `execution.stop_on_error` | `true` | booleano | — | — | validado; ainda sem consumo pelo runtime |
| `execution.allow_manual_mode` | `true` | booleano | — | administrador `false` proíbe `true` | em uso pelo runtime/CLI |
| `execution.max_call_depth` | `16` | inteiro 1–256 | níveis | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `execution.max_invocations_per_run` | `1000` | inteiro 1–100000 | invocações | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `execution.node_timeout_seconds` | `3600` | inteiro 1–86400 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `execution.run_timeout_seconds` | `86400` | inteiro 1–604800 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `execution.retry_max_attempts` | `1` | inteiro 1–10 | tentativas totais, incluindo a primeira | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `execution.retry_backoff_ms` | `1000` | inteiro 0–600000 | ms | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `execution.retry_jitter` | `true` | booleano | — | — | validado; ainda sem consumo pelo runtime |

### `templates`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `templates.strict_undefined` | `true` | booleano | — | — | em uso pelo runtime/CLI |
| `templates.allow_dowai_if_alias` | `true` | booleano | — | — | validado; ainda sem consumo pelo runtime |
| `templates.max_nesting_depth` | `32` | inteiro 1–64 | níveis | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `templates.max_render_bytes` | `1048576` | inteiro 1024–104857600 | bytes | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `templates.allowed_macros` | `[]` | lista vazia (não suportado nesta versão) | nomes; nesta versão só lista vazia (ADR 0004) | — | validado; ainda sem consumo pelo runtime |

### `security`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `security.allow_external_writes` | `false` | booleano | — | administrador `false` proíbe `true` | em uso pelo runtime/CLI |
| `security.require_approval_for_destructive` | `true` | booleano | — | administrador `true` impõe `true` | em uso pelo runtime/CLI |
| `security.allow_insecure_tls` | `false` | booleano | — | administrador `false` proíbe `true` | validado; ainda sem consumo pelo runtime |
| `security.allowed_paths` | `[]` | lista de textos | caminhos | interseção com a lista administrativa | validado; ainda sem consumo pelo runtime |
| `security.allowed_hosts` | `[]` | lista de textos | hosts | interseção com a lista administrativa | em uso pelo runtime/CLI |
| `security.allowed_profiles` | `[]` | lista de textos | perfis | interseção com a lista administrativa | em uso pelo runtime/CLI |

### `plugins`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `plugins.require_manifest` | `true` | booleano | — | administrador `true` impõe `true` | validado; ainda sem consumo pelo runtime |
| `plugins.allow_local_override` | `true` | booleano | — | administrador `false` proíbe `true` | validado; ainda sem consumo pelo runtime |
| `plugins.max_parallel_calls` | `4` | inteiro 1–64 | chamadas | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `plugins.default_timeout_seconds` | `300` | inteiro 1–86400 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `plugins.allow_network` | `false` | booleano | — | administrador `false` proíbe `true` | validado; ainda sem consumo pelo runtime |
| `plugins.allow_filesystem` | `false` | booleano | — | administrador `false` proíbe `true` | validado; ainda sem consumo pelo runtime |
| `plugins.require_idempotency_key` | `true` | booleano | — | administrador `true` impõe `true` | validado; ainda sem consumo pelo runtime |

### `logging`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `logging.level` | `"INFO"` | enum: `ERROR`, `WARNING`, `INFO`, `DEBUG` | — | — | validado; ainda sem consumo pelo runtime |
| `logging.audit_enabled` | `true` | booleano, sempre `true` | AUDIT essencial: sempre true | — | AUDIT essencial: sempre ativo |
| `logging.retention_days` | `90` | inteiro 1–3650 | dias | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `logging.rotation_max_mb` | `64` | inteiro 1–10240 | MiB | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `logging.compression` | `"gzip"` | enum: `none`, `gzip` | — | — | validado; ainda sem consumo pelo runtime |
| `logging.redaction_enabled` | `true` | booleano | — | administrador `true` impõe `true` | validado; ainda sem consumo pelo runtime |
| `logging.console_format` | `"text"` | enum: `text`, `json` | — | — | validado; ainda sem consumo pelo runtime |

### `lineage`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `lineage.enabled` | `false` | booleano | — | — | validado; ainda sem consumo pelo runtime |
| `lineage.emit_mode` | `"disabled"` | enum: `disabled`, `file` | — | — | validado; ainda sem consumo pelo runtime |
| `lineage.output_path` | `"lineage/openlineage.jsonl"` | texto não vazio | caminho relativo ao projeto | — | validado; ainda sem consumo pelo runtime |
| `lineage.namespace` | `"dowai"` | texto não vazio | — | — | validado; ainda sem consumo pelo runtime |
| `lineage.fail_on_emit_error` | `false` | booleano | — | — | validado; ainda sem consumo pelo runtime |

### `ai`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `ai.enabled` | `false` | booleano | — | administrador `false` proíbe `true` | validado; ainda sem consumo pelo runtime |
| `ai.default_profile` | `null` | texto não vazio ou `null` (herda) | nome do perfil | — | validado; ainda sem consumo pelo runtime |
| `ai.max_tokens` | `4096` | inteiro 1–1000000 | tokens | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `ai.max_cost_per_run` | `0.0` | número ≥ 0 | unidade monetária do provedor; 0 = nenhum custo permitido | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `ai.timeout_seconds` | `60` | inteiro 1–600 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `ai.allow_data_rows` | `false` | booleano | — | administrador `false` proíbe `true` | validado; ainda sem consumo pelo runtime |
| `ai.redaction` | `true` | booleano | — | administrador `true` impõe `true` | validado; ainda sem consumo pelo runtime |
| `ai.profiles` | `{}` | objeto | perfis de IA (objeto) | — | validado; ainda sem consumo pelo runtime |

### `sessions`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `sessions.conflict_policy` | `"error"` | enum: `error`, `wait_process` | — | — | em uso pelo runtime/CLI |
| `sessions.wait_process_timeout_seconds` | `300` | inteiro 1–86400 | s | não pode exceder o teto administrativo | em uso pelo runtime/CLI |
| `sessions.ttl_seconds` | `86400` | inteiro 60–31536000 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `sessions.max_variables` | `1000` | inteiro 1–100000 | variáveis | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `sessions.max_value_bytes` | `65536` | inteiro 1–10485760 | bytes | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `sessions.max_context_bytes` | `1048576` | inteiro 1–104857600 | bytes | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `sessions.allow_resume` | `true` | booleano | — | — | validado; ainda sem consumo pelo runtime |
| `sessions.persist_state` | `true` | booleano | — | — | validado; ainda sem consumo pelo runtime |

### `database`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `database.pool_max_connections` | `4` | inteiro 1–100 | conexões | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `database.connect_timeout_seconds` | `10` | inteiro 1–600 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `database.query_timeout_seconds` | `1800` | inteiro 1–86400 | s | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `database.fetch_batch_rows` | `10000` | inteiro 1–1000000 | linhas | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `database.write_batch_rows` | `10000` | inteiro 1–1000000 | linhas | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `database.max_transfer_bytes` | `1073741824` | inteiro 1–1099511627776 | bytes | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `database.tls_required` | `true` | booleano | — | administrador `true` impõe `true` | validado; ainda sem consumo pelo runtime |

### `python`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `python.dependency_lock_required` | `true` | booleano | — | administrador `true` impõe `true` | validado; ainda sem consumo pelo runtime |
| `python.install_tool` | `"uv"` | enum: `uv` | — | — | validado; ainda sem consumo pelo runtime |
| `python.venv_path` | `".venv"` | texto não vazio | caminho relativo ao projeto | — | validado; ainda sem consumo pelo runtime |
| `python.max_threads` | `4` | inteiro 1–256 | threads | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |
| `python.resource_enforcement` | `"best_effort"` | enum: `best_effort`, `require` | — | — | validado; ainda sem consumo pelo runtime |

### `state`

| Parâmetro | Default | Tipo e validação | Unidade | Teto administrativo | Estado |
|---|---|---|---|---|---|
| `state.backend` | `"sqlite"` | enum: `sqlite` | — | — | validado; ainda sem consumo pelo runtime |
| `state.path` | `"state/dowai.sqlite"` | texto não vazio | caminho relativo ao projeto | — | validado; ainda sem consumo pelo runtime |
| `state.busy_timeout_ms` | `5000` | inteiro 0–600000 | ms | — | validado; ainda sem consumo pelo runtime |
| `state.checkpoint_policy` | `"after_commit"` | enum: `after_commit` | — | — | validado; ainda sem consumo pelo runtime |
| `state.retention_days` | `90` | inteiro 1–3650 | dias | não pode exceder o teto administrativo | validado; ainda sem consumo pelo runtime |

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
