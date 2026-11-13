# Plugin oficial `files` (v1.0.0)

Plugin de leitura e escrita de arquivos CSV, JSON Lines e Parquet (plano §9.5, ENT-057).
Para usar, copie esta pasta para `<projeto>/plugins/files/` (escopo `project`) ou
`<workspace>/plugins/files/` (escopo `global`). Exige `plugins.allow_filesystem = true`.

## Métodos

| Alvo | Função | Uso |
|---|---|---|
| `files.read_rows` | `files:read_rows` | Conta linhas em streaming; devolve `rows`, `columns` e `preview`. |
| `files.write_rows` | `files:write_rows` | Grava até 10 000 linhas inline (lista de objetos). |
| `files.copy` | `files:copy` | Copia entre formatos em streaming, lote a lote. |

Formatos: `csv`, `jsonl` (JSON Lines; também `.ndjson`) e `parquet`. Sem `format`, a extensão decide.

## Parâmetros principais

- `base_dir` (obrigatório): raiz permitida. Todos os caminhos são relativos a ela.
- `path` (`read_rows`, `write_rows`), `source` e `target` (`copy`).
- `if_exists`: `replace` (padrão, escrita atômica) ou `fail` (`DOWAI_FILES_TARGET_EXISTS`).
- `batch_rows` (1 a 100 000, padrão 10 000) e `preview_rows` (0 a 100, padrão 20).

## Regras

- Caminho absoluto, `..`, componente vazio ou `.` e symlink em qualquer nível são recusados
  (`DOWAI_FILES_PATH_DENIED`).
- Escrita em arquivo temporário no diretório do destino, fsync e `os.replace`. Falha não deixa
  arquivo parcial nem temporário.
- Reexecutar `copy` com `if_exists=replace` produz o mesmo `checksum` (SHA-256 das linhas
  canônicas) e o mesmo número de linhas.
- CSV guarda texto: valores voltam como `str`. JSON Lines preserva tipos JSON. Parquet preserva
  o schema inferido no primeiro lote; mudança de tipo entre lotes falha (`DOWAI_FILES_IO_FAILED`).
- Parquet exige `pyarrow` no ambiente do run (`DOWAI_FILES_DEPENDENCY_MISSING` se ausente).
- SFTP e destinos fora da raiz não fazem parte desta versão (ver tarefa de autorização própria).

## Testes

```
python3 -I -m unittest discover -s tests/plugins/files -p "test_*.py"
```

Com `pyarrow` (ex.: `tests/contracts/python-dataframes/.dowai_venv/bin/python`): 13 testes OK.
Sem `pyarrow`: 10 OK e 3 Parquet pulados com motivo explícito.
