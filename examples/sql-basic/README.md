# sql-basic — SQL declarativo com fonte e `ref()` (ENT-083)

Exemplo do plano §17.1: a fonte `landing.clientes` e `landing.vendas` (declaradas em
`dowai_project.json`) alimentam `stg_clientes.sql`, e `faturamento.sql` referencia
`stg_clientes` com `ref()`. `dowai run faturamento.sql` planeja, executa o predecessor
`stg_clientes` e depois o alvo, tudo dentro do PostgreSQL.

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
python3 -I examples/sql-basic/run_example.py
```

O runner copia o exemplo para um diretório temporário, gera o `workspace.json` a partir das
variáveis acima (nenhuma credencial vai para o repositório), cria as tabelas de origem no schema
`dowai_example_sql_basic_landing`, executa `dowai run faturamento.sql` duas vezes (a segunda com
`--yes`, pois substituir relações existentes exige confirmação) e remove tudo ao final.

Comando manual equivalente, depois de montar um `workspace.json` com o perfil `warehouse`
(ver `docs/CONFIGURATION.md`):

```bash
dowai run faturamento.sql --json          # primeira execução
dowai run faturamento.sql --yes --json    # reexecução com substituição
```

## Resultado esperado

Saída do runner:

```text
OK    primeira execução SUCCEEDED
OK    primeira execução: resultado esperado
OK    segunda execução (--yes: substitui as relações) SUCCEEDED
OK    segunda execução (--yes: substitui as relações): resultado esperado
sql-basic: OK
```

Conteúdo de `faturamento` após cada execução (`cliente_id|nome|total`):

```text
1|Ana|150.50
2|Bruno|200.00
```

A cliente `Carla` não aparece porque não tem vendas (`join`).

## Limitações

- Os critérios de aceite da ENT-083 exigem execução no CI; o CI hospedado depende da ENT-010 (bloqueada).
  O exemplo foi validado localmente contra PostgreSQL (ver `_gestao/EVIDENCIAS.md`).
- O runner não é incluído na suíte padrão `tests/examples` porque requer PostgreSQL.
