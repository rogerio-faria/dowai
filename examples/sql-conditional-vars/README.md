# sql-conditional-vars — SQL condicional com variáveis (ENT-083)

Parte SQL do plano §17.2: `faturamento.sql` escolhe entre duas consultas com
`{% if var('carga_total', false) == true %}`. `{{ def_var('carga_total', false) }}` define o
padrão apenas quando a variável está ausente; um valor passado pela CLI (`--vars`) prevalece.

Este exemplo cobre apenas a execução SQL com `--vars`. A chamada `env.run("faturamento.sql")` a
partir de um modelo Python não está implementada (retorna `DOWAI_NOT_IMPLEMENTED`); ver a tarefa
ENT-114 em `_gestao/tarefas-pendentes/`.

## Pré-requisitos

- PostgreSQL acessível com um usuário que possa criar schemas e tabelas no banco indicado.
- `psql` no `PATH` e o binário `dowai` em `target/debug/dowai` (ou `DOWAI_BIN`).
- O exemplo grava no PostgreSQL (`security.allow_external_writes = true` em `dowai_project.json`).

## Como executar

Na raiz do repositório:

```bash
export DOWAI_EXAMPLE_PG_USER=<usuario>
export DOWAI_EXAMPLE_PG_DATABASE=<banco>
# opcionais: DOWAI_EXAMPLE_PG_HOST (padrão 127.0.0.1), DOWAI_EXAMPLE_PG_PORT (5432),
#            DOWAI_EXAMPLE_PG_TLS (required | disabled, use disabled só em loopback),
#            DOWAI_EXAMPLE_PG_PASSWORD (vazia se a autenticação for confiável)
python3 -I examples/sql-conditional-vars/run_example.py
```

O runner copia o exemplo para um diretório temporário, gera o `workspace.json` a partir das
variáveis acima, cria a tabela de origem `dowai_example_sql_conditional_landing.vendas` e executa
três vezes:

1. `dowai run faturamento.sql --json` (sem variáveis);
2. `dowai run faturamento.sql --yes --vars '{"carga_total": true}' --json`;
3. `dowai run faturamento.sql --yes --json` (sem variáveis, sem `--session`).

Ao final, remove a tabela `faturamento` e o schema de origem.

Comandos manuais equivalentes, após montar um `workspace.json` com o perfil `warehouse`:

```bash
dowai run faturamento.sql --json
dowai run faturamento.sql --yes --vars '{"carga_total": true}' --json
dowai run faturamento.sql --yes --json
```

## Resultado esperado

Saída do runner:

```text
OK    sem variáveis (padrão false): SUCCEEDED
OK    sem variáveis (padrão false): resultado esperado
OK    --vars {"carga_total": true}: SUCCEEDED
OK    --vars {"carga_total": true}: resultado esperado
OK    sem variáveis após execução com variável (sem --session): SUCCEEDED
OK    sem variáveis após execução com variável (sem --session): resultado esperado
sql-conditional-vars: OK
```

Conteúdo de `faturamento` (`cliente_id|total`) por cenário:

| Cenário | Ramo | Resultado |
|---|---|---|
| sem variáveis | `else` (`valor >= 100`) | `1|100.00` e `2|200.00` |
| `--vars '{"carga_total": true}'` | `then` (todas as vendas) | `1|150.50` e `2|200.00` |
| sem variáveis, sem `--session` | `else` | `1|100.00` e `2|200.00` |

O terceiro cenário confirma que uma execução nova não herda variáveis de uma execução anterior
quando não há `--session`.

## Limitações

- Os critérios de aceite da ENT-083 exigem execução no CI; o CI hospedado depende da ENT-010 (bloqueada).
  O exemplo foi validado localmente contra PostgreSQL (ver `_gestao/EVIDENCIAS.md`).
- Sessões (`session start` e `--session`) não são demonstradas aqui; o fluxo com sessão compartilhada
  exige `env.run` a partir de Python (ENT-114).
- O runner não é incluído na suíte padrão `tests/examples` porque requer PostgreSQL.
