# DOWAI

O DOWAI executa pipelines de dados com modelos SQL e Python no mesmo grafo de dependências, contra PostgreSQL. Você escreve os modelos, a ferramenta descobre a ordem de execução, grava com segurança e registra tudo o que aconteceu.

## Modelos SQL

Um arquivo `.sql` por tabela ou view, dentro de `models/`. O nome do arquivo é o nome do modelo.

**1. `stg_pedidos.sql`: leitura da origem (tabela)**

```sql
{{ config(materialized='table') }}
select id, cliente_id, valor, status
from {{ source('landing', 'pedidos') }}
where status <> 'cancelado'
```

**2. `faturamento_mensal.sql`: agregação sobre outro modelo (tabela)**

```sql
{{ config(materialized='table') }}
select cliente_id, sum(valor) as total, count(*) as pedidos
from {{ ref('stg_pedidos') }}
group by cliente_id
```

**3. `clientes_top.sql`: view sobre o resultado (view)**

```sql
{{ config(materialized='view') }}
select cliente_id, total
from {{ ref('faturamento_mensal') }}
where total >= 1000
```

As origens (`source`) são declaradas uma vez, em `dowai_project.json`:

```json
"sources": {
  "landing": {
    "schema": "landing",
    "tables": { "pedidos": "pedidos" }
  }
}
```

## Modelos Python

Um arquivo `.py` com uma função `model(env)`. Ele pode ler modelos SQL do mesmo run, ler origens declaradas, gravar dados e guardar variáveis de sessão.

**1. Gravar o resultado de um modelo SQL em outro destino**

```python
def model(env):
    env.depends_on("faturamento_mensal")
    linhas = [dict(r) for r in env.ref("faturamento_mensal").iter_rows(batch_size=500)]
    env.write(linhas, target="warehouse", table="faturamento_exportado", mode="replace")
    env.setVar("exportados", len(linhas))
    return None
```

`mode="replace"` troca a tabela de destino e, por isso, exige `--yes` na execução.

**2. Carga incremental com watermark declarado pelo próprio modelo**

```python
PEDIDOS = [
    {"id": 1, "atualizado_em": 10, "status": "ok"},
    {"id": 2, "atualizado_em": 20, "status": "ok"},
]

def model(env):
    marca = env.getVar("pedidos_marca", 0)
    novos = [p for p in PEDIDOS if p["atualizado_em"] > marca]
    if novos:
        env.write(novos, target="warehouse", table="pedidos_historico", mode="append")
        env.setVar("pedidos_marca", max(p["atualizado_em"] for p in novos))
    env.setVar("carregados_agora", len(novos))
    return None
```

A marca só avança depois que a gravação é confirmada. Uma segunda execução na mesma sessão não carrega nada repetido.

**3. Validação de qualidade antes de publicar**

```python
def model(env):
    linhas = list(env.source("landing", "pedidos").iter_rows(batch_size=500))
    invalidas = [r for r in linhas if r["valor"] is None or r["valor"] < 0]
    env.setVar("linhas_invalidas", len(invalidas))
    if invalidas:
        raise ValueError(f"{len(invalidas)} pedidos com valor inválido; nada foi publicado")
    env.write(linhas, target="warehouse", table="pedidos_validados", mode="append")
    return None
```

Se a validação falhar, a execução termina com erro e a tabela de destino não recebe nenhuma linha.

## Como executar

```bash
dowai validate . --json            # confere schema, referências, ciclos e origens
dowai plan all --json              # mostra a ordem calculada
dowai run all --yes --json         # executa; --yes confirma substituições
dowai runs list                    # histórico local de execuções
dowai retry <run_id> --failed-only --yes   # reexecuta só o que falhou
```

Configuração do projeto e do perfil do PostgreSQL: veja [docs/CONFIGURATION.md](docs/CONFIGURATION.md). A senha é lida de variável de ambiente e nunca fica em arquivo.

## Lacunas preenchidas em relação ao dbt

| Necessidade | No dbt | No DOWAI |
|---|---|---|
| SQL e Python no mesmo pipeline | Modelos Python dependem da plataforma do warehouse | SQL e Python no mesmo grafo e na mesma sessão |
| Variáveis entre modelos | Não há estado compartilhado nativo entre modelos | `setVar`/`getVar`, persistidas na sessão |
| Proteção contra sobrescrita | Substituição é o padrão do `dbt run` | Escrita externa desligada por padrão; substituição exige `--yes` |
| Carga incremental | Lógica `is_incremental()` no SQL | Watermark declarado pelo modelo, avançado só após commit |
| Reconstrução completa | Recriação direta | `--full-refresh --yes` com staging e troca em uma transação; recusa se o schema divergir |
| Falhas transitórias | `dbt retry` reexecuta nós que falharam | Retry com backoff e jitter em falhas transitórias do PostgreSQL, dentro da execução, e `dowai retry` por execução |
| Auditoria | Logs de execução | Diário local com códigos `DOWAI_*` estáveis; guarda hashes, nunca valores |
| Escrita direta em Python | Não controlada pelo framework | Política opcional que recusa drivers ou ORMs usados para gravar dados |
| Credenciais em modelos | Variáveis de ambiente ou perfil | Modelo recebe um handle com nome e tipo, sem URL, usuário ou senha |

**Onde o dbt ainda está à frente:** ecossistema de pacotes, maturidade de testes e documentação, e suporte a vários warehouses. O DOWAI hoje executa modelos SQL somente em PostgreSQL.

## Estado atual

O DOWAI está em desenvolvimento. Implementado e verificado localmente: planejamento e validação offline, modelos SQL em PostgreSQL (tabelas e views), modelos Python em venv do projeto, variáveis de sessão, cargas incrementais, `--full-refresh`, retries, e o plugin `files` (CSV, JSON Lines e Parquet).

Ainda não disponível: funções de IA, lineage OpenLineage, instaladores e pacotes de release. O plugin `sftp` não foi testado contra servidor real. O CI hospedado em Linux e Windows ainda não foi executado. Detalhes, limites e evidências: [docs/STATUS.md](docs/STATUS.md). Roteiro: [docs/ROADMAP.md](docs/ROADMAP.md).

## Build

Requisitos: toolchain Rust com Cargo.

```bash
cargo build --workspace
cargo test --workspace
```

No Linux sem `pkg-config`, defina antes: `OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include`.

## Documentação

- [Configuração](docs/CONFIGURATION.md), [API](docs/API.md), [compatibilidade](docs/COMPATIBILITY.md), [operação](docs/OPERATIONS.md), [segurança](docs/SECURITY.md)
- [Contribuição](CONTRIBUTING.md) e [política de segurança](SECURITY.md)
- Requisitos normativos: [plano_mestre.md](plano_mestre.md)
