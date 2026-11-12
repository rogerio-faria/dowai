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
