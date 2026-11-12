# ADR 0004: Subconjunto dbt e aliases de template

- Status: **Accepted** — o usuário pediu para usar a proposta do GP (ENT-003, 2026-11-12). A lista abaixo é a decisão vigente; a expansão além dela exige nova decisão.
- Date: 2026-11-12
- Decide: D12 (compatibilidade dbt por versão, subconjunto mínimo testado) e D16 (aliases `{{ if }}` normalizados explicitamente pelo parser).
- Referências: `plano_mestre.md` §7.2 (subconjunto dbt e diagnóstico), §18 (D12, D16); `src/dowai-core/src/templates.rs`; `src/dowai-core/tests/template_contract.rs`.

## Contexto

O plano não promete compatibilidade integral com dbt. Exige um subconjunto explícito, diagnóstico `SUPPORTED`/`PARTIAL`/`UNSUPPORTED`, e renderização por parser allowlist, sem executar Python ou Jinja arbitrário.

Estado verificado em 2026-11-12:

- `templates.rs` implementa tokenização de `{% %}` (padrão) e `{{ }}` (alias DOWAI) para `if`, `elif`, `else`, `endif` e `def_var`. Expressões `{{ ref(...) }}`, `{{ source(...) }}` e `{{ config(...) }}` são preservadas como chamadas estáticas, validadas em `render_sql_model`. Qualquer outra expressão de `{{ }}` ou `{% %}` é rejeitada com `DOWAI_TEMPLATE_INVALID`.
- Condições aceitas: `var('nome')` ou `var('nome', padrão)`, opcionalmente comparadas com `==` ou `!=` a escalar JSON; ou truthiness do valor. Não há `and`, `or`, `not`, chamadas de função ou acesso a atributos.
- `config(materialized='table'|'view')` é o único `config` aceito. `materialized='incremental'`, `unique_key`, `ephemeral` e outras chaves são rejeitados.
- **Não há leitura de `dbt_project.yml` nem de `sources.yml`** no código (nenhuma ocorrência em `src/`). Fontes são declaradas no projeto DOWAI.
- Não há suporte a macros, hooks, snapshots, seeds, packages ou pacotes de terceiros.

## Decisão

### D12 — Subconjunto dbt

**SUPPORTED (testado por fixtures, comportamento garantido):**

| Recurso | Forma aceita | Teste |
|---|---|---|
| `ref` | `{{ ref('nome') }}` com um literal de string; resolve modelo do plano; ambiguidade e inexistência são erro | `sql_model_rendering_resolves_refs_and_extracts_table_materialization`, `sql_model_rendering_rejects_unknown_refs_and_unsupported_config` |
| `source` | `{{ source('fonte', 'tabela') }}` com fonte declarada no projeto; não declarada é erro | `sql_model_rendering_resolves_declared_external_sources`, `sql_model_rendering_rejects_undeclared_external_sources` |
| `config` | `{{ config(materialized='table') }}` ou `'view'` (padrão `table`) | `sql_model_rendering_defaults_to_table_and_accepts_views` |
| Condicionais | `if`, `elif`, `else`, `endif` com `var()` (com ou sem padrão) e truthiness | `elif_and_else_select_one_branch`, `nested_conditions_select_the_expected_sql` |
| Variáveis | `def_var('nome', valor)` define só se ausente | `def_var_sets_absent_values_for_later_conditions`, `def_var_does_not_overwrite_existing_values` |
| Comentários | `{# ... #}` ignorados | `comments_are_ignored_and_unterminated_delimiters_fail` |

**PARTIAL (declarado, sem leitura de arquivo dbt):** nenhum recurso PARTIAL nesta versão. `dbt_project.yml` e `sources.yml` **não são lidos**; qualquer projeto que dependa deles recebe diagnóstico `UNSUPPORTED` ou `PARTIAL` pela futura verificação de compatibilidade (ENT-035). Leitura parcial de `sources.yml` (apenas nomes de fonte e tabela) só entra em nova decisão.

**UNSUPPORTED (erro `DOWAI_TEMPLATE_INVALID` ou diagnóstico, sem fallback silencioso):**

- Macros arbitrárias (`{% macro %}`), `{% for %}`, `{% set %}`, filtros e chamadas Jinja fora da allowlist.
- Saída de expressão (`{{ var('x') }}` para interpolação de valor) — variáveis não são interpoladas no SQL.
- `env_var()`, `target.*`, `adapter.*`, `run_query()`, chamadas Python.
- `ref()` com mais de um argumento; `source()` com argumentos diferentes de dois literais.
- `config` com `materialized` diferente de `table`/`view` (inclui `incremental` e `ephemeral`) e com chaves fora de `materialized` (inclui `unique_key`). Incremental só entra com teste transacional e semântica definidos (plano §7.2; ENT-032/ENT-033).
- Packages, hooks (`pre_hook`/`post_hook`), snapshots, seeds, testes e exposures do dbt.
- Operadores lógicos (`and`, `or`, `not`) e expressões aninhadas em condições.
- Dialetos e drivers não testados: `UNSUPPORTED`, sem execução.

### D16 — Aliases de template

O DOWAI aceita, como equivalentes, os dois delimitadores para as mesmas diretivas:

| Forma padrão | Alias DOWAI | Exemplo |
|---|---|---|
| `{% if cond %}` | `{{ if cond }}` | `{{ if var('mode') == 'full' }}select 1{{ endif }}` |
| `{% elif cond %}` | `{{ elif cond }}` | `{{ elif var('mode') == 'delta' }}select 2` |
| `{% else %}` | `{{ else }}` | `{{ else }}select 3` |
| `{% endif %}` | `{{ endif }}` | `{{ endif }}` |
| `{% def_var('x', 1) %}` | `{{ def_var('x', 1) }}` | `{{ def_var('limit', 10) }}` |

Regras:

1. Um alias é normalizado para a diretiva correspondente pelo parser (`parse_directive`); não há tradução textual.
2. Os dois delimitadores podem ser misturados no mesmo arquivo; o resultado renderizado é idêntico ao da forma padrão.
3. `{{ ref(...) }}`, `{{ source(...) }}` e `{{ config(...) }}` são **chamadas estáticas**, não aliases de diretiva.
4. Qualquer outra expressão dentro de `{{ }}` é rejeitada.

## Alternativas consideradas

- **Suportar Jinja completo (ou `jinja2` do Python):** execução de código arbitrário no parser, sem garantia de paridade com dbt. Rejeitado (plano §7.2).
- **Usar dbt como biblioteca para compilar SQL:** acopla a versão do dbt e exige ambiente Python; sem matriz homologada. Rejeitado nesta fase; a comparação com dbt de versão fixada fica para ENT-035.
- **Suportar `and`/`or` já:** ampliaria a gramática sem fixtures de paridade. Adiado; exige nova decisão.
- **Ler `dbt_project.yml`/`sources.yml` como PARTIAL já:** sem testes de paridade, criaria compatibilidade alegada sem evidência. Rejeitado.

## Consequências

- Projetos dbt que usem macros, `incremental`, hooks, seeds ou snapshots não rodam e devem receber diagnóstico `UNSUPPORTED` (ENT-035).
- A lista acima é um contrato: ampliar qualquer item exige teste novo e atualização deste ADR.
- A documentação de usuário (ENT-080, `docs/COMPATIBILITY.md`) deve refletir esta tabela, sem prometer compatibilidade integral com dbt.

## Testes associados

Executados em 2026-11-12 (E-029):

- `cargo test -p dowai-core --test template_contract --locked`: 24 passed, 0 failed. Inclui os testes novos `every_dowai_alias_form_matches_its_standard_form` (cobre D16 nas cinco diretivas) e `dbt_constructs_outside_the_supported_subset_are_rejected` (cobre os casos UNSUPPORTED de macro, `for`, `env_var`, `ref` com dois argumentos e `config` com `incremental`, `unique_key` e `ephemeral`).

Pendentes para tarefas futuras (não alegados como cobertos): diagnóstico `SUPPORTED`/`PARTIAL`/`UNSUPPORTED` com localização e sugestão (`dowai compatibility check`, ENT-035); comparação com dbt de versão fixada (ENT-004 e ENT-035).
