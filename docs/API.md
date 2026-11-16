# API pública — CLI, SDK Python e contratos

Documento de referência da superfície pública do DOWAI nesta revisão (`dowai 0.1.0`).
Status real por comando e limitações estão em `docs/STATUS.md`; este arquivo não promete
nada além do que foi verificado. Requisitos de origem: `plano_mestre.md` §6.1 e §20.1.

Verificação deste documento: `cargo build -p dowai-cli --locked` com as variáveis
`OPENSSL_*` documentadas em `STATUS.md`, e execução real dos comandos marcados como
"verificado localmente" nos exemplos abaixo, em `examples/automatic-dag` e em um
diretório temporário. Os exemplos **não** foram executados no CI (critério pendente, ver
ENT-078 e ENT-010).

## 1. CLI

### 1.1 Status dos comandos de §6.1

| Comando | Status | Observação |
|---|---|---|
| `dowai --version` | Implementado | Verificado localmente (`dowai 0.1.0`). |
| `dowai init <name> [--workspace PATH] [--json]` | Parcial | Cria projeto e workspace sem sobrescrever configuração existente. Verificado em diretório temporário. |
| `dowai doctor [--json] [--check-connections]` | Parcial | Checa configuração, runtime Python e conexão SQL. Verificado com `--json`. |
| `dowai config show --effective [--json] [--project-dir PATH]` | Parcial | Camadas admin/usuário/workspace/projeto com origem. Catálogo completo de parâmetros não implementado (ver `CONFIGURATION.md`). Verificado. |
| `dowai config migrate <path> --output <novo.json>` | Implementado | Lê YAML legado sem alterá-lo; recusa sobrescrever saída. |
| `dowai validate [PATH] [--strict] [--json]` | Parcial | Schema, referências, ciclos, declarações de source e sintaxe de template. Verificado: `valid: true` em `examples/automatic-dag`. |
| `dowai plan <targets>... [--vars JSON] [--project-dir PATH] [--json]` | Parcial | Ordenação automática offline. Verificado: `stg_vendas.sql` antes de `faturamento.sql`. |
| `dowai plan --manual <targets>... [--vars JSON] [--json]` | Parcial | Ordem informada pelo usuário com dependências. Verificado com `faturamento.sql stg_vendas.sql`. |
| `dowai run <targets>|all [--profile NAME] [--vars JSON] [--session ID] [--json]` | Parcial | SQL (tabelas/views no PostgreSQL) e Python em venv do projeto. Exige banco configurado; verificado localmente contra PostgreSQL de teste (E-110, D-114). Full refresh e demais adaptadores: ver §1.3. |
| `dowai run --manual <targets>... [--session ID]` | Parcial | Mesma restrição de `run`. |
| `dowai run <target> --full-refresh --yes` | Parcial | PostgreSQL: reconstrói as tabelas/views selecionadas em staging no mesmo banco e troca pelo nome numa única transação; sem `--yes` recusa com exit 5 antes de qualquer escrita; colunas (nome e tipo, em ordem) divergentes do destino falham com exit 3 e mantêm a relação anterior. Não altera schema automaticamente. |
| `dowai session start [--project-dir PATH] [--json]` | Parcial | Verificado: retorna `session_id`, `status: ACTIVE`, TTL. |
| `dowai session show <session_id> [--json]` | Parcial | Verificado. |
| `dowai session end <session_id> [--json]` | Parcial | Verificado: `status: ENDED`. |
| `dowai runs list [--limit N] [--json]` | Parcial | Diário local de execuções. Verificado (padrão 20). |
| `dowai runs show <run_id> [--json]` | Parcial | Execução com invocações. |
| `dowai logs <run_id> [--node NODE] [--level LEVEL] [--json]` | Parcial | Árvore de invocações do diário. Não há mensagens livres. |
| `dowai retry <run_id> [--failed-only] [--vars JSON] [--yes] [--json]` | Parcial | Reexecuta execução `FAILED` ou `CANCELLED` como nova execução. Variáveis não são armazenadas e devem ser informadas novamente. Execução de `--full-refresh` reaplica o modo (staging e swap, checagem de schema) e exige `--yes`; runs gravadas antes do schema 6 do journal são tratadas como não-full-refresh. |
| `dowai plugins list [--scope project\|global\|all]` | Parcial | Apenas leitura de manifestos. |
| `dowai plugins info <id>` | Parcial | Apenas leitura de manifestos. |
| `dowai plugins sync` | Parcial | Gera stubs `.pyi` em `.dowai/stubs`, sem importar código de plugin. |
| `dowai plugins resolve <namespace.method>` | Parcial | Apenas resolve; não executa código de plugin. |
| `dowai plugins run <namespace.method> --input JSON --idempotency-key KEY` | Parcial | Executa plugin em subprocesso isolado, com AUDIT e idempotência. Sandbox de SO não implementado (ENT-108). |
| `dowai compatibility check <project_path> [--json]` | Parcial | Classifica constructs dbt como `SUPPORTED`, `PARTIAL` ou `UNSUPPORTED`; apenas lê arquivos. |
| `dowai ia generateLineage <file_or_model>` | Não implementado | Ausente do CLI. Fora das rodadas aprovadas (M5). |
| `dowai ia generateHumanDocs <file_or_model>` | Não implementado | Ausente do CLI. Fora das rodadas aprovadas (M5). |
| `dowai ai review <run_id>` | Não implementado | Ausente do CLI. Fora das rodadas aprovadas (M5). |
| `dowai benchmark <file_or_model>` | Não implementado | Ausente do CLI. Fora das rodadas aprovadas (backlog em `TAREFAS.md`). |

Comandos não implementados **não aparecem** no `--help` como funcionais e retornam
`error: unrecognized subcommand` (verificado para `ia`, `ai` e `benchmark`).

### 1.2 Alvos, modos e variáveis

- Alvos aceitos: arquivos `.sql` ou `.py`, `all`, ou ID inequívoco de modelo. Caminhos são
  confinados à raiz autorizada; symlinks e path traversal são recusados.
- **Modo automático** (padrão): a ordem vem do grafo de `ref()` e `env.depends_on()`.
- **Modo manual** (`--manual`): a ordem é a informada, desde que respeite as dependências.
- **Variáveis**: `--vars '<JSON objeto>'`. Os valores são efêmeros; o diário registra apenas
  o número de variáveis. Variáveis de sessão persistem somente após sucesso de todos os
  modelos da execução.
- **Sessão**: `--session <session_id>` reutiliza variáveis da sessão. Sessões expiram por TTL.

### 1.3 Execução real e limitações

- `run` escreve no PostgreSQL apenas com adaptador configurado. Substituir relações
  existentes exige `--yes`.
- Adaptadores MySQL e demais origens/destinos: não implementados nesta revisão.
- Cancelamento (Ctrl+C/SIGTERM) retorna `CANCELLED`, exit 130 e `DOWAI_CANCELLED`.

### 1.4 Saída JSON e códigos de erro

Com `--json`, toda resposta é um objeto JSON com `schema_version` e, em falha, `status`,
`error_code` e `message`. Exemplo real de falha (`plan --manual` com caminho inexistente):

```json
{"details":null,"error_code":"DOWAI_PLAN_INVALID","message":"Model not found: ...","schema_version":2,"status":"FAILED"}
```

Códigos de saída (`dowai-core/src/error.rs`):

| Código | Significado |
|---|---|
| 0 | Sucesso |
| 2 | Entrada, configuração, parser ou plano inválido; também uso incorreto da CLI |
| 3 | Falha de execução de modelo |
| 4 | Falha de conexão externa |
| 5 | Política, permissão ou segurança negada |
| 6 | Funcionalidade não implementada |
| 130 | Cancelado |

### 1.5 Exemplos verificados localmente

Executados em `examples/automatic-dag` (sem banco, sem SQL executado):

```bash
dowai validate examples/automatic-dag --json
dowai plan all --project-dir examples/automatic-dag --json
dowai plan --manual faturamento.sql stg_vendas.sql --project-dir examples/automatic-dag --json
dowai compatibility check examples/automatic-dag --json
dowai session start --project-dir examples/automatic-dag --json
```

Observação: `doctor`, `run`, `retry` e `logs` resolvem o projeto pelo diretório atual
(`cd` antes); `doctor` não aceita `--project-dir`. `run` **não foi executado** nesta revisão.

## 2. SDK Python (`dowai`)

Pacote em `src/dowai-sdk`. Exporta `DowaiClient`, `Env`, `Dataset`, `ExecutionBackend`,
`RunResult`, `DowaiExecutionError` e `__version__`. O SDK chama o binário `dowai` com `--json`;
o caminho é obtido de `DOWAI_BIN`, do `PATH` ou do argumento `binary=`.

### 2.1 `DowaiClient`

```python
from dowai import DowaiClient

client = DowaiClient("examples/automatic-dag", binary=None, timeout_seconds=None)
plan = client.plan(["all"], manual=False, variables={"x": 1})
result = client.run(["faturamento.sql"], manual=False, profile=None,
                    session_id=None, variables=None, confirm_destructive=False)
retry = client.retry("<run_id>", failed_only=True, variables=None, confirm_destructive=False)
runs = client.runs(limit=20)
detail = client.run_detail("<run_id>")
logs = client.logs("<run_id>", node=None, level="INFO")
resolved = client.plugin_resolve("namespace.method")
```

- `plan` retorna o mesmo JSON de `dowai plan --json` (teste de paridade AC034).
- `run` e `retry` retornam `RunResult`; recusas do engine voltam como resultado de falha.
- `runs(limit)` aceita 1 a 1000.
- Falha de sistema (binário ausente, versão incompatível, timeout) lança `DowaiExecutionError`
  com código estável.

### 2.2 `Env` (SDK dentro de modelos Python)

**Contrato da função `model` (ENT-125).** A função de entrada aceita uma das assinaturas:

```python
def model(env):                      # modelo sem origem (compatível com versões anteriores)
    ...

def model(env, origem=None):         # recebe o modelo SQL que chamou via run() (ENT-126)
    if origem is None:               # obrigatório tratar None quando executado diretamente
        ...
    df = origem.to_pandas(max_rows=1000000)
```

- `origem` é um dataset lazy com o mesmo contrato de `env.ref(...)`. Quando não há modelo pai (`dowai run x.py`, ou `env.run("x.py")` sem origem), `origem` é `None`.
- Passar uma origem a um modelo com assinatura `model(env)` falha com `DOWAI_MODEL_SIGNATURE_INVALID`, em vez de ignorar o pai silenciosamente. Assinaturas com mais de dois parâmetros obrigatórios também são recusadas com esse código, antes de qualquer efeito.
- Nesta versão, o runtime ainda não passa origem a partir de `{{ run(...) }}` (ENT-126 pendente); o contrato do worker já está pronto.

Disponível no modelo Python executado pelo runtime:

| Método | Status |
|---|---|
| `setVar(name, value)`, `getVar(name, default)`, `unsetVar(name)` | Implementado como sobreposição local, publicada só se a execução inteira tiver sucesso. |
| `manual()` (contexto) | Implementado. |
| `run(target, mode=None, raise_on_error=True)` | Implementado para `.py` (subprocesso supervisionado, com limites de profundidade e contagem) e para `.sql` (executa no PostgreSQL do run, com perfil e sessão do run pai; ENT-114). |
| `plan(target, mode=None)`, `validate()` | Implementado; mesmo JSON do CLI. |
| `ref(name).schema()` | Implementado para modelos SQL de uma execução mista SQL+Python (somente leitura, apenas relações declaradas em `depends_on`; limite de 10 000 requisições por execução). |
| `ref(name).iter_rows(batch_size)` | Implementado em lotes de até 10 000 linhas, em ordem física; recusado para views (`DOWAI_DATASET_UNAVAILABLE`). |
| `ref(name).select(*cols)`, `.filter(col, op, value)`, `.iter_batches(batch_size)` | Implementados (ENT-041). Projeção e comparações (`=`, `!=`, `<`, `<=`, `>`, `>=`, `is_null`, `is_not_null`) em `bool`, inteiros, `float` e texto rodam no SQL com parâmetros tipados. `=`/`!=` em tipos lidos como texto (ex.: `numeric`, datas, UUID) são aplicados pelo worker sobre a forma textual e geram o aviso `DOWAI_DATASET_FILTER_NOT_PUSHED` em `warnings`. Comparações ordenadas nesses tipos são recusadas com `DOWAI_DATASET_FILTER_UNSUPPORTED`; projeção inválida com `DOWAI_DATASET_INVALID_QUERY`. Objeto lazy não pode ser serializado e expira ao fim do model (`DOWAI_DATASET_EXPIRED`). |
| `generateCsv(data, path, **options)` | Implementado (ENT-050). Recebe um `Dataset` (lido em lotes) ou um iterável de mapeamentos com as mesmas chaves; grava UTF-8 por streaming em arquivo temporário e renomeia ao final, sem arquivo parcial em caso de falha. Opções: `delimiter` (padrão `,`), `header` (padrão `True`), `null` (padrão vazio) e `timezone` (nome IANA; aplicado somente a `datetime` com fuso, valores de texto passam sem conversão). Destino: caminho relativo ao projeto ou absoluto dentro dele; recusa `..`, links simbólicos em qualquer componente, diretórios reservados (`models`, `.dowai`, `.venv`, `dowai_project.json`) e destinos externos com `DOWAI_POLICY_DENIED` (destinos externos/SFTP ficam para ENT-057). Retorna o número de linhas. |
| `ref(name).to_arrow(max_rows)`, `.to_pandas(max_rows)`, `.to_polars(max_rows)` | Implementados (ENT-047). Materializam o dataset (lido em lotes) com mapeamento explícito de tipos PostgreSQL: `bool`, `int2/4/8`, `float4/8`, texto, `numeric` (decimal exato, escala preservada), `date`, `timestamp` (sem fuso) e `timestamptz` (convertido para UTC). Nulos são preservados. Tipos sem mapeamento (ex.: `json`, `uuid`, `bytea`) são recusados com `DOWAI_DATASET_TYPE_UNSUPPORTED` (use `select`); `max_rows` (padrão `1000000`, limite `10000000`) recusa com `DOWAI_DATASET_TOO_LARGE`; biblioteca ausente no venv do projeto recusa com `DOWAI_DATASET_LIBRARY_UNAVAILABLE`; `NaN`/infinito recusados com `DOWAI_DATASET_VALUE_INVALID`. Exige `pyarrow`, `pandas` ou `polars` em `python.dependencies`. |
| `write(rows, target, table, mode, keys, confirm_replace)` | Implementado (ENT-048) para perfis `postgres` e, desde ENT-034, `mysql` (`target` é o nome do perfil). Modos `append`, `replace` (exige `confirm_replace=True`) e `upsert` (exige `keys` com índice único exato no destino). Uma transação por sessão de escrita, atômica somente no banco de destino: uma transferência PostgreSQL→MySQL são duas transações independentes, sem atomicidade entre bancos (a escrita no MySQL permanece confirmada se o restante da execução falhar depois). No MySQL o upsert usa `INSERT ... ON DUPLICATE KEY UPDATE` e dispara por qualquer índice único do destino; booleanos viram `0/1`. Exige `security.allow_external_writes`. Destino não declarado em `workspace.json` é recusado com `DOWAI_CONFIG_INVALID` antes de qualquer conexão (ENT-127); quando o modelo propaga a falha de escrita, o código reportado é o do destino (por exemplo `DOWAI_CONFIG_INVALID`), e não o genérico `DOWAI_PYTHON_FAILED`. Duplicidade, `WHERE` e escolha do modo são responsabilidade do usuário. Exemplo: `examples/sql-run-post`. |
| Dependências instaladas pelo modelo | Não implementado. |

Erros de runtime são `DowaiExecutionError` com `code`, `hint`, `run_id` e `invocation_id`.

### 2.3 Modo manual x automático no SDK

```python
client.plan(["stg_vendas.sql", "faturamento.sql"], manual=True)   # ordem informada
client.plan(["all"])                                              # ordem automática
```

## 3. Limitações conhecidas

- Não há CI hospedado que execute estes exemplos (ENT-010 em `tarefas-autorizacao/`).
- Os testes com PostgreSQL exigem `DOWAI_TEST_PG_*` (P-09); execução de `run` contra banco
  não foi verificada nesta revisão.
- Windows não foi verificado a partir deste ambiente.
- A superfície de plugins não tem sandbox de SO (ENT-108).

## 4. Referências

- `docs/STATUS.md`: estado real por área.
- `docs/CONFIGURATION.md`: parâmetros, precedência e defaults.
- `docs/OPERATIONS.md`: sessões, backup, logs e upgrades.
- `docs/SECURITY.md`: limites de segurança e permissões.
- `docs/COMPATIBILITY.md`: sistemas, versões e constructs dbt.
