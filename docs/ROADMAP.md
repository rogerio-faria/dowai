# Roadmap

This sequence follows the milestone gates in [plano_mestre.md](../plano_mestre.md).

1. **M0/M1 — contracts and offline planning:** initial project/config
   resolution and static planning are implemented and tested locally; remaining
   work includes broader schemas, CI/platform coverage, the full config
   catalog, and integration of persistent sessions with execution.
2. **M2 — SQL execution:** SQL-only PostgreSQL `table`/`view` execution,
   `ref()` resolution, declared `source()` relations, and per-model
   transactional replacement are implemented locally; session-backed runtime
   verification for refs, sources, variables, and replacement approval passes
   against PostgreSQL 17.4 with loopback TLS disabled. Remote TLS, production
   credentials, MySQL, and cross-database E2E remain outstanding. The PostgreSQL
   adapter provides TLS-by-default connections and bound parameters.
3. **M3 — Python runtime:** the first Python-only, session-backed execution
   path is implemented with versioned JSON IPC, project-local venv selection,
   a configurable timeout, process-tree termination, variable APIs, and
   explicit unsupported-operation errors. Imperative `env.run()` between Python
   models is implemented (invocation tree, variable overlay, cycle and depth
   limits, `env.manual()`). Mixed SQL/Python execution, nested SQL, datasets,
   dependency management/authorization, SDK parity, cancellation, a persistent
   invocation journal, richer error context, and execution isolation remain
   outstanding.
4. **M4/M5 — integrations:** authorized plugins, data transfer, audit,
   OpenLineage, semantic dictionary, and optional AI.
5. **M6/M7 — robustness and release:** recovery, idempotency, performance,
   security/platform matrices, and installable Linux/Windows packages.

No later-stage feature is represented as complete merely because its command
name or schema appears in the master plan.
