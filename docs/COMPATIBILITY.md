# Compatibilidade dbt — DOWAI

Documento inicial da compatibilidade parcial com projetos dbt (plano_mestre §7.2, P19).
O DOWAI **não** promete compatibilidade integral com dbt.

## Comando

```text
dowai compatibility check <project_path> [--json]
```

- Apenas lê arquivos: não renderiza, não executa SQL/Python e não importa código.
- Classificação por construto: `SUPPORTED`, `PARTIAL` ou `UNSUPPORTED`, com caminho relativo à raiz do projeto, linha (quando conhecida) e sugestão.
- Status agregado: `UNSUPPORTED` se houver algum construto não suportado; senão `PARTIAL` se houver algum parcial; senão `SUPPORTED`.
- Código de saída: `0` quando o relatório é produzido (construtos não suportados são diagnósticos, não falhas); erro de projeto inválido segue o contrato de `dowai_core::exit_codes`.
- `--json` produz somente o payload, validado por `specs/compatibility.schema.json` (`schema_version` 1).

## Matriz inicial

| Construto | Status | Código de diagnóstico |
|---|---|---|
| `ref('modelo')` | SUPPORTED | — |
| `source('fonte', 'tabela')` | SUPPORTED | — |
| `config(materialized='table')` / `config(materialized='view')` | SUPPORTED | — |
| `def_var` (extensão DOWAI) | SUPPORTED | — |
| `run("arq.py")` (extensão DOWAI, ENT-124) | PARTIAL | Aceita somente no fim do arquivo SQL, após todo o SQL, com literal de caminho relativo `.py` (sem `..`, sem barra invertida, sem caractere de controle). Múltiplas chamadas executam em ordem. Renderização não executa nada. A execução pós-materialização ainda não existe (ENT-126): o runtime recusa o modelo com `DOWAI_NOT_IMPLEMENTED` antes de executar qualquer SQL. |
| `{% if %}` / `{% elif %}` / `{% else %}` (subconjunto de `var()`) | PARTIAL | `DOWAI_COMPAT_CONDITIONAL_SUBSET` |
| `config(materialized='incremental')` e demais materializações | UNSUPPORTED | `DOWAI_COMPAT_MATERIALIZATION_UNSUPPORTED` |
| `config(...)` com chaves diferentes de `materialized` | UNSUPPORTED | `DOWAI_COMPAT_CONFIG_UNSUPPORTED` |
| `config(pre_hook=…/post_hook=…)` e `on-run-start`/`on-run-end` | UNSUPPORTED | `DOWAI_COMPAT_HOOK_UNSUPPORTED` |
| Chamada de macro, ex.: `{{ dbt_utils.star() }}` | UNSUPPORTED | `DOWAI_COMPAT_MACRO_UNSUPPORTED` |
| `var()` como valor impresso no SQL | UNSUPPORTED | `DOWAI_COMPAT_EXPRESSION_UNSUPPORTED` |
| Expressão de template não reconhecida | UNSUPPORTED | `DOWAI_COMPAT_EXPRESSION_UNSUPPORTED` |
| Bloco Jinja não suportado (`{% macro %}`, `{% for %}`, `{% set %}` etc.) | UNSUPPORTED | `DOWAI_COMPAT_BLOCK_UNSUPPORTED` |
| Diretórios `macros/`, `snapshots/`, `seeds/` e arquivo `packages.yml` | UNSUPPORTED | `DOWAI_COMPAT_MACROS_UNSUPPORTED`, `DOWAI_COMPAT_SNAPSHOTS_UNSUPPORTED`, `DOWAI_COMPAT_SEEDS_UNSUPPORTED`, `DOWAI_COMPAT_PACKAGES_UNSUPPORTED` |

Comentários `{# … #}` são ignorados. Modelos Python (`.py`) não são avaliados por esta verificação, pois não têm equivalente dbt. Arquivos `.sql` em symlinks não são seguidos.

## Limitações conhecidas

- **Sem comparação com dbt.** Nenhuma versão de dbt está fixada nem há fixtures comparadas com a saída do dbt. Por isso não há afirmação de paridade de resultados (AC022 segue PARTIAL).
- **Dialetos e drivers não são classificados.** A verificação não avalia o adapter de destino; dialetos não testados devem ser tratados como `UNSUPPORTED` na execução (plano §7.2), ainda sem diagnóstico estático.
- **Incremental** depende de contrato explícito com `unique_key` e teste transacional (ENT-066); até lá é `UNSUPPORTED`.
- **Análise textual.** A classificação segue a sintaxe de template aceita pelo renderizador DOWAI. Conteúdo SQL dentro de blocos não é interpretado.

## Matriz de versões testadas (ENT-080)

Esta matriz lista somente versões com evidência de teste executado e registrado em `_gestao/EVIDENCIAS.md`. Versões sem evidência não aparecem como suportadas. A matriz de CI hospedada (`.github/workflows/ci.yml`) ainda não foi executada remotamente (ENT-010 bloqueada, sem remoto Git); por isso a coluna "Evidência" indica apenas validação local.

| Componente | Versão testada | Evidência local | Limitações |
|---|---|---|---|
| SO | Ubuntu 24.04.5 LTS (Linux 6.8.0-149) | Suítes Rust e Python (E-089, E-111, E-115) | Sem validação de Windows |
| Rust | stable, rustc/cargo 1.99.1 | `cargo fmt --check`, `clippy -D warnings`, suítes do workspace (E-089, E-111) | Sem MSRV declarada |
| Python | 3.12.3 | SDK `unittest`: 48 testes OK (E-089) | — |
| Python | 3.10.22 | SDK `unittest`: 48 testes OK (E-089) | — |
| PostgreSQL | 16.15 | Testes E2E com `DOWAI_TEST_PG_*` (E-107, E-108); quick start e exemplo `manual-sequence` (E-115, E-116) | Validação local, sem CI; PostgreSQL 17 (job de CI) não foi executado |
| MySQL | 8.0.46 | Contrato `dowai-mysql` 14 OK, `mysql_source_e2e` 3 OK, `write_mysql_e2e` 6 OK (E-111, ENT-034/ENT-116) | Instância efêmera em loopback, TLS desabilitado por loopback; sem CI; MySQL 8.4 e 9.x não testados |

### Versões não validadas

- **Windows** (`windows-latest` na definição de CI): nenhuma execução local ou hospedada. Não é plataforma validada.
- **PostgreSQL 17** (job `postgres-e2e` do CI): não executado.
- **Python 3.11 e superiores**: não testados. O parser Python do plano cobre até ~3.11 (D-048).
- **Outras versões de MySQL e de PostgreSQL**: não testadas.

### Divergência documental

O ADR `docs/ADR/0006-matriz-de-versoes-suportadas.md` marca MySQL como não suportado. A decisão D-115 registra MySQL como suportado, com a versão 8.0.46 testada localmente. Esta matriz segue a decisão D-115 e o registro de evidências; o ADR deve ser atualizado por nova decisão registrada em `_gestao/DECISOES.md` ou por novo ADR.
