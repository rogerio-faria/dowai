# Roadmap

This sequence follows the milestone gates in [plano_mestre.md](../plano_mestre.md).

1. **M0/M1 — contracts and offline planning:** initial project/config
   resolution and static planning are implemented and tested locally; remaining
   work includes broader schemas, CI/platform coverage, the full config
   catalog, and integration of persistent sessions with execution.
2. **M2 — SQL execution:** PostgreSQL, MySQL, Oracle and SQL Server `table`/`view` execution,
   `ref()` resolution, declared `source()` relations, and per-model replacement
   are implemented locally. SQL source reads use the selected execution
   profile; a different declared profile is rejected rather than staged or
   converted. Nested SQL runs reuse the selected SQL profile. Session-backed
   PostgreSQL runtime verification passes; opt-in MySQL SQL E2E verification
   remains dependent on database test settings. Oracle and SQL Server were tested
   against live encrypted connections; their documented timeout, DDL and
   full-refresh limitations remain. The PostgreSQL adapter provides
   TLS-by-default connections and bound parameters.
3. **M3 — Python runtime:** the first Python-only, session-backed execution
   path is implemented with versioned JSON IPC, project-local venv selection,
   a configurable timeout, process-tree termination, variable APIs, and
   explicit unsupported-operation errors. Imperative `env.run()` between Python
   models is implemented (invocation tree, variable overlay, cycle and depth
   limits, `env.manual()`). Nested SQL, datasets,
   dependency management/authorization, SDK parity, cancellation, a persistent
   invocation journal, richer error context, and execution isolation remain
   outstanding.
4. **M4/M5 — integrations:** authorized plugins, data transfer, audit,
   OpenLineage, semantic dictionary, and optional AI.
5. **M6/M7 — robustness and release:** recovery, idempotency, performance,
   security/platform matrices, and installable Linux/Windows packages.

No later-stage feature is represented as complete merely because its command
name or schema appears in the master plan.
