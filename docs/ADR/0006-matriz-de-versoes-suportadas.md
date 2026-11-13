# ADR 0006: Matriz de versões suportadas

- Status: **Accepted** — decisão do usuário registrada em `_gestao/DECISOES.md` (D-092, 2026-11-13).
- Date: 2026-11-13
- Decide: D01 (`plano_mestre.md` §18): versões de SO, Python, Rust, PostgreSQL e MySQL suportadas para GA em Windows e Linux.
- Referências: `plano_mestre.md` §15.3; `.github/workflows/ci.yml`; `src/dowai-sdk/pyproject.toml`.
- Tarefa: ENT-004.

## Contexto

O plano exige uma matriz testável que permita o GA em Windows e Linux. A matriz deve ser a menor possível, usar versões já presentes no projeto e declarar explicitamente o que não é coberto por job de CI.

## Decisão

Regra: **uma versão só é listada como suportada se houver job de CI que a execute**. Versões validadas apenas localmente aparecem como tal, com a lacuna declarada. Versões sem job e sem validação são marcadas como não suportadas.

| Componente | Versão suportada | Job de CI (`ci.yml`) | Validação local (2026-11-13) | Situação |
|---|---|---|---|---|
| SO | `ubuntu-latest` | `rust`, `python-sdk`, `postgres-e2e` | Ubuntu 24.04.5 LTS (Linux 6.8.0-149) | Suportado |
| SO | `windows-latest` | `rust`, `python-sdk` | não executada | Suportado na definição de CI; **execução hospedada ainda não ocorreu** (depende de ENT-010) |
| Python | 3.12 | `python-sdk` (ubuntu e windows) | Python 3.12.3: `unittest` do SDK, 48 OK | Suportado |
| Python | 3.10 | `python-sdk` (ubuntu e windows) — adicionado nesta tarefa | Python 3.10.22: `unittest` do SDK, 48 OK | Suportado (`requires-python >= 3.10`); execução em CI pendente de ENT-010 |
| Rust | stable (edição 2021) | `rust` (`dtolnay/rust-toolchain@stable`) | rustc/cargo 1.99.1 | Suportado. Sem MSRV declarada |
| PostgreSQL | 17 | `postgres-e2e` (`postgres:17`) | não validado: o cluster local é 16.15 e os testes com PostgreSQL dependem de `DOWAI_TEST_PG_*` (P-09) | Suportado pela CI; validação local pendente |
| PostgreSQL | 16 | nenhum | cluster local 16.15 existe, mas sem testes executados | **Não suportado** nesta matriz (uso local apenas) |
| MySQL | qualquer | nenhum | nenhuma (adapter ainda não existe; ENT-034) | **Não suportado** nesta fase; versão a fixar quando ENT-034 for concluída |

## Consequências

- O job `python-sdk` passa a rodar em uma matriz `os × python` (4 combinações). O workflow foi validado apenas quanto à sintaxe YAML; **não foi executado em runner hospedado**.
- Nenhuma plataforma é declarada *validada* além do que a evidência local comprova. Windows e a matriz de CI hospedada aguardam ENT-010.
- A diferença entre PostgreSQL 16 local e 17 de CI é conhecida e deve ser reconciliada quando P-09 for resolvido.
- MySQL fica explicitamente fora da GA atual. A inclusão exige nova decisão e job de CI (ENT-034).
- Qualquer alteração desta matriz exige novo ADR ou atualização registrada em `DECISOES.md`.
