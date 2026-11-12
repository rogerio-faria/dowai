# Changelog

Este arquivo segue o formato de [Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/)
e a política de versionamento [SemVer 2.0.0](https://semver.org/lang/pt-BR/),
com as regras abaixo.

## Política de versionamento

- Versões são `MAJOR.MINOR.PATCH`. Os pacotes Rust do workspace e o SDK Python
  (`dowai-sdk`) são versionados juntos enquanto o projeto estiver na série `0.x`.
- Enquanto a versão for `0.x`, mudanças incompatíveis podem ocorrer em `MINOR`,
  mas **sempre** devem ser listadas em `Breaking changes` com migração.
- A partir de `1.0.0`, mudanças incompatíveis exigem `MAJOR`.
- Compatibilidade pública inclui: comandos e flags da CLI, formato de saída
  `--json` (campos e envelopes), códigos de erro `DOWAI_*`, exit codes,
  schemas em `specs/`, formato de `dowai_project.json`, configuração em
  `docs/CONFIGURATION.md` e API pública do SDK Python.
- Campos novos e opcionais em saídas JSON são mudanças compatíveis (`MINOR`).
  Remoção, renomeação ou mudança de significado são incompatíveis.
- O SDK recusa motor de `major.minor` diferente com `DOWAI_VERSION_INCOMPATIBLE`.

## Breaking changes e migrações

Cada entrada incompatível deve conter: o que mudou, quem é afetado, o passo de
migração e, quando houver, o comando ou arquivo de migração disponível (por
exemplo `dowai config migrate`). Ainda não há mudanças incompatíveis publicadas.

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
