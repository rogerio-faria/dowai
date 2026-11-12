# ADR 0002: Estado, sessões e transações de variáveis

- Status: **Accepted** — aceito pelo usuário em 2026-11-12 (ENT-001).
- Date: 2026-11-12
- Decide: D02 (backend de estado/sessão), D14 (persistência e locking de sessões), D15 (commit de variáveis por invocação bem-sucedida).
- Referências: `plano_mestre.md` §4.4, §18, §4 (sessões e variáveis), §18 D02/D14/D15.

## Contexto

O plano exige sessões com variáveis compartilhadas entre `env.run()` e comandos CLI sucessivos (`--session`), revisões versionadas com origem, TTL, limites de tamanho, bloqueio de escrita concorrente sem last-writer-wins silencioso e recuperação após falhas. Efeitos externos (banco, arquivos, e-mail) não participam da transação de variáveis.

Estado atual no código (verificado em 2026-11-12):

- `src/dowai-core/src/sessions.rs` usa SQLite (`rusqlite`, bundled) em `state/sessions.sqlite3`, com transações `IMMEDIATE`, TTL (`expires_at_ms`) e expiração que remove variáveis recuperáveis.
- `src/dowai-core/src/runs.rs` usa SQLite separado (`state/runs.sqlite3`) com `busy_timeout` de 5 s e esquema versionado.
- O CLI aplica TTL padrão de 86 400 s (`DEFAULT_SESSION_TTL_SECONDS`).

## Decisão proposta

1. **D02 — Backend:** SQLite local transacional, um arquivo por tipo de estado (sessões separadas de runs). Migrações de esquema versionadas, com rejeição de versões mais novas.
2. **D14 — Persistência e locking:** sessão retomável apenas por `session_id` dentro do mesmo usuário do SO, com TTL. Escritas usam transação `IMMEDIATE` e `busy_timeout`. Conflito de escrita concorrente na mesma chave é erro (`sessions.conflict_policy = "error"`); nunca last-writer-wins.
3. **D15 — Commit de variáveis:** cada invocação recebe snapshot mais overlay dos ancestrais. Alterações pendentes só são publicadas na sessão após término bem-sucedido da invocação raiz. Falha, timeout ou cancelamento descarta o overlay não confirmado. Um erro filho tratado por `try` descarta apenas o overlay desse filho.

## Alternativas consideradas

- **Arquivo JSON por sessão:** simples, mas sem transação atômica entre processos nem bloqueio confiável. Rejeitado.
- **Servidor/daemon de estado:** melhor para concorrência, mas exige serviço permanente, o que é operação sensível e fora do escopo autorizado. Rejeitado.
- **Last-writer-wins:** mais simples, porém perde escritas silenciosamente, contrariando o plano. Rejeitado.
- **Commit por invocação (cada `env.run()` publica):** quebra a atomicidade da árvore de invocações e deixa estado parcial após falha raiz. Rejeitado.

## Consequências

- Estado durável fica em arquivos SQLite sob o diretório do projeto, com permissões do usuário.
- Efeitos externos não são revertidos com as variáveis: falha do chamador pode deixar banco/arquivos já confirmados. Isso deve ser documentado e testado; não se alega rollback distribuído.
- Concorrência entre processos depende de `busy_timeout`; contenção prolongada retorna erro de estado, não espera indefinida.
- Migrações de esquema exigem testes de dados legados.

## Testes associados

Testes que já existem e exercitam as decisões (executados em ENT-001, ver EVIDENCIAS.md):

- D02/migração: `legacy_database_migrates_without_data_loss`, `new_database_has_versioned_schema`, `newer_schema_versions_are_rejected` (runs.rs).
- D14/TTL e limites: `expired_sessions_are_not_usable_and_their_variables_are_deleted`, `variable_limits_are_enforced_without_partial_writes`, `ending_a_session_removes_its_recoverable_variables`.
- D14/conflito: `concurrent_write_to_the_same_key_is_a_conflict_and_keeps_the_winner`, `a_conflict_on_one_key_publishes_none_of_the_keys`, `deleting_and_recreating_a_key_is_still_a_conflict`.
- D15 (parcial): `published_revisions_record_origin_without_values`, `sessions_persist_typed_variables_across_store_instances`.

Testes planejados para D15 (ainda não implementados; previstos em ENT-025 e ENT-026, sem alterar a decisão):

- Overlay de ancestrais em árvore de `env.run()` e descarte de overlay após falha da raiz.
- Erro filho tratado por `try` descartando apenas o overlay do filho.
- Efeito externo confirmado mesmo com falha posterior da raiz (teste de contrato).
