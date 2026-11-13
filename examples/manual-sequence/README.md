# manual-sequence — sequência manual com tratamento de erro (ENT-084)

Exemplo do plano §17.3. O modelo `sequencia.py` abre `env.manual()`, executa
`arq1.sql`, tenta `arq10.sql` dentro de `try` e depois executa `arq7.py`. Nenhum
`env.run` cria aresta no DAG estático: a árvore de invocações guarda a ordem e a
relação pai-filho.

Outro modelo, `sequencia_sem_tratamento.py`, faz a mesma sequência sem `try`. A
falha de `arq10.sql` interrompe o modelo e `arq7.py` nunca é iniciado.

| Arquivo | Papel |
|---|---|
| `models/arq1.sql` | SQL que cria a relação `arq1` (ok). |
| `models/arq10.sql` | SQL que falha de propósito (divisão por zero). |
| `models/arq7.py` | Python executado depois da sequência; marca `arq7_executado`. |
| `models/sequencia.py` | Sequência com `try`: a falha de `arq10` é tolerada. |
| `models/sequencia_sem_tratamento.py` | Sequência sem `try`: a falha propaga. |

## Como executar

Pré-requisitos: PostgreSQL acessível com um usuário que possa criar tabelas no
banco indicado; `psql`; `python3` com `venv`; binário `dowai` em
`target/debug/dowai` (ou `DOWAI_BIN`).

Na raiz do repositório:

```bash
export DOWAI_EXAMPLE_PG_USER=<usuario>
export DOWAI_EXAMPLE_PG_DATABASE=<banco>
# opcionais: DOWAI_EXAMPLE_PG_HOST, DOWAI_EXAMPLE_PG_PORT, DOWAI_EXAMPLE_PG_TLS
# (required | disabled), DOWAI_EXAMPLE_PG_PASSWORD
python3 -I examples/manual-sequence/run_example.py
```

O runner copia o exemplo para um diretório temporário, cria um venv sem pacotes
(os modelos usam só a biblioteca padrão), gera o `workspace.json` e remove a
tabela `arq1` e o diretório temporário ao final.

## Resultado esperado

```text
OK    sequencia.py com try: SUCCEEDED
OK    sequencia.py: variáveis publicadas ['arq7_executado', 'carga_total', 'falha_tolerada']
OK    sequencia.py: arq10.sql falhou e a falha foi tolerada
OK    sequencia.py: arq7.py executou depois da falha tolerada
OK    sequencia_sem_tratamento.py: FAILED com DOWAI_EXECUTION_FAILED (exit 3)
OK    sem try: arq7.py não foi iniciado
manual-sequence: OK
```

## Diferenças em relação ao plano

- O plano §17.3 usa `env.log.warn(...)`. O worker atual não tem `env.log`
  (não implementado), então o exemplo registra a decisão com
  `env.setVar("falha_tolerada", True)`.
- O critério "executa no CI" de ENT-084 depende de ENT-010, bloqueada por falta de
  repositório remoto. Por isso a validação é local, contra PostgreSQL real (D-124).
