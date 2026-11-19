# Exemplos de leitura e gravação em bancos

PostgreSQL, MySQL, Oracle e SQL Server são escolhidos por perfil. O comportamento depende de qual
adaptador cada modelo usa: o runtime não copia nem converte automaticamente
dados entre bancos.

## 1. Configurar perfis e fontes

Em `workspace.json`, cadastre as conexões. As credenciais ficam em variáveis
de ambiente indicadas por `password_env`:

```json
{
  "schema_version": 2,
  "profiles": {
    "pg_warehouse": {
      "type": "postgres",
      "host": "pg.example.test",
      "port": 5432,
      "user": "dowai",
      "database": "analytics",
      "password_env": "DOWAI_PG_PASSWORD",
      "tls": "required"
    },
    "mysql_mart": {
      "type": "mysql",
      "host": "mysql.example.test",
      "port": 3306,
      "user": "dowai",
      "database": "mart",
      "password_env": "DOWAI_MYSQL_PASSWORD",
      "tls": "required"
    },
    "oracle_erp": {
      "type": "oracle",
      "host": "oracle.example.test",
      "port": 1521,
      "user": "dowai",
      "database": "SERVICE_NAME",
      "password_env": "DOWAI_ORACLE_PASSWORD",
      "tls": "oracle_native"
    },
    "sqlserver_mart": {
      "type": "sqlserver",
      "host": "sqlserver.example.test",
      "port": 1433,
      "user": "dowai",
      "database": "mart",
      "password_env": "DOWAI_SQLSERVER_PASSWORD",
      "tls": "required"
    }
  },
  "defaults": {
    "execution": {
      "default_profile": "mysql_mart"
    }
  }
}
```

Declare tabelas de origem em `dowai_project.json`. O `profile` é opcional:
Python pode selecioná-lo na chamada a `env.source()`. Para MySQL, `schema`
deve ser o banco configurado no perfil.

```json
{
  "schema_version": 2,
  "name": "sales",
  "models_dir": "models",
  "config": {
    "security": {
      "allow_external_writes": true
    }
  },
  "sources": {
    "erp": {
      "schema": "public",
      "tables": {
        "orders": "orders"
      }
    },
    "catalog": {
      "profile": "mysql_mart",
      "schema": "mart",
      "tables": {
        "products": "products"
      }
    }
  }
}
```

Os nomes `erp` e `catalog` são nomes lógicos; os valores em `tables` são os
nomes reais das tabelas.

## 2. SQL: origem e destino na mesma conexão

Um modelo SQL executa inteiramente no perfil selecionado. Suas chamadas
`source()` devem apontar para esse mesmo perfil. Por exemplo,
`models/customer_orders.sql` lê e materializa no PostgreSQL:

```sql
{{ config(materialized='table') }}

SELECT id AS order_id, customer_id
FROM {{ source('erp', 'orders') }}
```

Execute com:

```bash
dowai run customer_orders.sql --profile pg_warehouse
```

Não há cópia para outro banco nem conversão automática de tipos. Se uma fonte
SQL declarar um perfil diferente do perfil usado pelo modelo, a execução falha
quando essa fonte for referenciada.

`{{ run("outro_modelo.sql") }}` mantém o perfil SQL selecionado; o modelo SQL
chamado continua executando no mesmo servidor e conexão do run.

## 3. SQL chamando Python para gravar em outro banco

O modelo SQL continua usando seu próprio perfil. Se precisar enviar o resultado
a outro banco, faça a transformação explicitamente em Python e escolha o
destino com `env.write()`.

No final de `models/customer_orders.sql`:

```sql
{{ run("grava_mysql.py") }}
```

Em `models/grava_mysql.py`, `origem` representa o resultado SQL do modelo
chamador. A conversão de tipos e a forma dos dados ficam sob controle do
usuário:

```python
def model(env, origem=None):
    if origem is None:
        return None

    rows = []
    for row in origem.iter_rows(batch_size=500):
        rows.append({
            "order_id": int(row["order_id"]),
            "customer_id": int(row["customer_id"]),
        })

    if rows:
        env.write(
            rows,
            target="mysql_mart",
            table="orders_copy",
            mode="append",
        )
    return None
```

Execute o conjunto pelo perfil SQL de origem:

```bash
dowai run customer_orders.sql --profile pg_warehouse
```

`env.write()` escolhe o banco e o perfil de destino; a escrita não implica
conversão automática entre os tipos dos bancos.

## 4. Python: escolher a origem e o destino

Um modelo Python pode selecionar uma origem declarada com `env.source()`. O
argumento `profile` é opcional: quando informado, substitui o perfil da
declaração; quando omitido, vale o perfil declarado ou o perfil padrão.

```python
def model(env):
    # Lê "erp.orders" usando explicitamente o perfil PostgreSQL.
    source = env.source("erp", "orders", profile="pg_warehouse")
    columns = source.schema()

    rows = []
    for row in source.iter_rows(batch_size=500):
        rows.append({
            "order_id": int(row["id"]),
            "customer_id": int(row["customer_id"]),
        })

    if rows:
        env.write(
            rows,
            target="mysql_mart",
            table="orders_copy",
            mode="append",
        )

    env.setVar("order_columns", [column["name"] for column in columns])
    env.setVar("orders_read", len(rows))
    return None
```

O usuário decide como transformar os dados antes de gravá-los. Para fontes
MySQL, Oracle e SQL Server, a tabela precisa ter chave primária para leitura
ordenada em lotes; `filter()` nessas fontes ainda não é suportado. Oracle e
SQL Server retornam valores não nulos como texto; o usuário deve convertê-los
para os tipos apropriados ao destino.

Por exemplo, após declarar `oracle_orders` no projeto com schema, tabela e
perfil Oracle, Python pode transferir seus dados para uma tabela existente no
SQL Server:

```python
def model(env):
    source = env.source("oracle_orders", "orders", profile="oracle_erp")
    for batch in source.iter_batches(batch_size=500):
        rows = [{"id": int(row["ID"]), "label": row["LABEL"]} for row in batch]
        if rows:
            env.write(rows, target="sqlserver_mart", table="orders_copy", mode="append")
```

Declare `orders` com o nome real da tabela Oracle e use os nomes reais de
suas colunas. Cada chamada de `env.write` é uma transação independente; uma
falha posterior não desfaz lotes já gravados. O Oracle exige Oracle Client e,
para `oracle_native`, a configuração `TNS_ADMIN` descrita no README.

`env.write()` aceita `append`, `replace` e `upsert`. `replace` exige
`confirm_replace=True`; `upsert` requer chaves compatíveis com um índice único
do destino. A tabela de destino deve existir e a política
`security.allow_external_writes` deve estar habilitada. Cada gravação é
transacional somente no banco de destino.

Para detalhes, consulte [docs/CONFIGURATION.md](docs/CONFIGURATION.md) e
[docs/API.md](docs/API.md).
