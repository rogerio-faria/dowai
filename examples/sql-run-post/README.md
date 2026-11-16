# sql-run-post — SQL que chama Python após a materialização (ENT-126)

`models/carga.sql` materializa `carga` no perfil PostgreSQL padrão e, depois,
executa `{{ run("transforma.py") }}`, que está no fim do arquivo. O Python recebe
o modelo como `origem` (dataset lazy, lido com `iter_batches`).

| Arquivo | Papel |
|---|---|
| `models/carga.sql` | Três linhas e a chamada `run()` no fim do arquivo. |
| `models/transforma.py` | Usa `origem`; trata `origem is None` para execução direta. |
| `dowai_project.json` | Projeto mínimo; `allow_external_writes` habilitado. |

Execução:

```
dowai run carga.sql      # origem = carga
dowai run transforma.py  # execução direta: origem = None
```

Regras: `run()` só pode aparecer no fim do arquivo SQL; o caminho é relativo a
`models/`; falha do Python falha a execução sem reverter a tabela `carga`.
