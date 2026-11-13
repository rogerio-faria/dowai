# python-pandas-cross-db — Python com pandas e outro banco (ENT-084)

Exemplo do plano §17.4. O modelo `comissao.py` lê `faturamento` (SQL, no perfil
`warehouse`) com `env.ref("faturamento").to_pandas()`, calcula a comissão de 5%
com pandas e grava o resultado no perfil `financeiro` (outro banco PostgreSQL)
com `env.write(df, target="financeiro", table="comissoes", mode="append")`.

| Arquivo | Papel |
|---|---|
| `models/faturamento.sql` | Origem: três clientes e seus totais. |
| `models/comissao.py` | Lê com `to_pandas()`, calcula e grava no destino. |
| `dowai_project.json` | Sem dependências declaradas; `security.allowed_profiles` lista os dois perfis. |
| `requirements.in` / `requirements.lock` | Versão fixada de pandas, com hashes (gerados com `uv pip compile`). |

## Como executar

Pré-requisitos: dois bancos PostgreSQL acessíveis com o mesmo usuário (origem e
destino podem estar no mesmo servidor); `psql`; `uv` (versão fixada em ENT-004,
ou `UV_BIN`); binário `dowai` em `target/debug/dowai` (ou `DOWAI_BIN`).

Na raiz do repositório:

```bash
export DOWAI_EXAMPLE_PG_USER=<usuario>
export DOWAI_EXAMPLE_PG_DATABASE=<banco de origem>
export DOWAI_EXAMPLE_FIN_DATABASE=<banco de destino>
# opcionais: DOWAI_EXAMPLE_PG_HOST, DOWAI_EXAMPLE_PG_PORT, DOWAI_EXAMPLE_PG_TLS
# (required | disabled), DOWAI_EXAMPLE_PG_PASSWORD
python3 -I examples/python-pandas-cross-db/run_example.py
```

O runner copia o exemplo para um diretório temporário e cria `.venv` com
`uv pip sync --require-hashes` a partir de `requirements.lock`. O runtime não
instala pacotes: por isso `dowai_project.json` não declara dependências, e o runtime
recusa `python.dependencies` com `DOWAI_NOT_IMPLEMENTED`. O runner também cria a
tabela `comissoes` no destino e a remove, junto com `faturamento`, ao final.

## Resultado esperado

```text
OK    tabela de destino criada no perfil financeiro
OK    comissao.py: SUCCEEDED
OK    faturamento.sql (predecessor por env.ref) executou antes do modelo
OK    destino com as comissões esperadas: ['1|1500|75', '2|800|40', '3|120|6']
python-pandas-cross-db: OK
```

## Limites deste exemplo

- Os dados são pequenos. Para volume maior, o plano §17.4 sugere substituir
  `to_pandas()` por batches Arrow (`to_arrow` ou `iter_batches`); `to_pandas()`
  tem limite de linhas (`max_rows`) e recusa conjuntos maiores.
- Origem e destino são transações independentes. Uma falha no destino depois da
  leitura não desfaz nada na origem, e uma escrita já confirmada no destino não é
  revertida se a execução falhar depois (ver `docs/CONFIGURATION.md`).
- O critério "executa no CI" de ENT-084 depende de ENT-010 (sem repositório
  remoto). A validação é local, contra PostgreSQL real (D-124).
