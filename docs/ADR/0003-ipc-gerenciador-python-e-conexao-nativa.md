# ADR 0003: IPC, gerenciador Python e conexão nativa

- Status: **Accepted** — decidido pelo GP na execução da ENT-002, dentro da autorização geral e da autorização explícita da tarefa em 2026-11-12. Nenhuma mudança de compatibilidade do protocolo v1 é feita por este ADR; o usuário pode revisar as decisões a qualquer momento.
- Date: 2026-11-12
- Decide: D03 (transporte IPC Rust↔Python), D04 (gerenciador Python e lockfile), D05 (conexão nativa para Pandas sem URL com segredo).
- Referências: `plano_mestre.md` §8.1, §8.2, §8.4, §18 (D03, D04, D05), linhas de protocolo em §4 (IPC versionado) e §8.3; `specs/ipc.schema.json`; `src/dowai-runtime/src/python_runtime.rs`; `src/dowai-sdk/`.

## Contexto

O plano exige que Python rode em subprocesso supervisionado, com IPC versionado, timeout, cancelamento e limites de tamanho; que dados volumosos não sejam transferidos como tabela inteira em JSON, usando Arrow IPC ou streaming comprovado; que dependências sejam declaradas em `dowai_project.json`, fixadas em lockfile reproduzível e instaladas somente de índice autorizado; e que a conexão para Pandas seja um objeto de capacidade, não uma URL com segredo exposta.

Estado atual verificado em 2026-11-12:

- **Transporte:** NDJSON (uma mensagem JSON por linha) sobre `stdin`/`stdout` do processo Python, `protocol_version` 1, criado com `Command::new` e pipes herdados (`python_runtime.rs`). Não há porta de rede, socket nomeado nem token de autenticação; a confiança depende do pipe herdado pelo pai.
- **Limites:** cada linha é limitada a `MAX_IPC_BYTES` = 20 MiB; canais de leitura e resposta são `sync_channel` (backpressure); lotes de linhas (`dataset_rows`) são limitados a 10 000 linhas por chamada (`iter_rows`, B-09.2).
- **Contrato:** `specs/ipc.schema.json` descreve `hostRequest`, `workerCall`, `workerDataset` (operações `schema` e `rows`), `workerFinal`, `hostCallReply` e `hostDatasetReply`, com `additionalProperties: false`.
- **Gerenciador Python:** `validate_python_runtime` rejeita dependências e políticas diferentes de `declared_and_authorized` ("instalação ainda não disponível"). Não há lockfile nem venv de projeto. O SDK (`dowai-sdk`) não tem dependências de runtime (`dependencies = []`).
- **Ferramentas locais:** Python do sistema 3.12.3, sem `pyarrow`, `pandas` ou `polars` instalados. `uv` 0.13.7 disponível em `~/.local/bin/uv` (instalado no espaço do usuário).
- **Testes executados nesta tarefa (ver EVIDENCIAS.md, E-028):** `cargo test -p dowai-core --test schema_contracts` (4 passed); `cargo test -p dowai-runtime` (todos passed, 1 ignored por requisito de PostgreSQL); SDK Python `unittest` (25 OK).

## Decisão

### D03 — Transporte IPC

1. **Controle em JSON validado, por linha.** Mantemos NDJSON + `protocol_version` como contrato de controle (requisição, chamadas aninhadas, `dataset` de schema, final). O protocolo v1 permanece compatível; mudanças são aditivas.
2. **Dados volumosos em Arrow IPC (protocolo v2, decisão de direção).** Lotes de linhas passam a ser frames binários com prefixo de tamanho (`u32` big-endian + bytes de um stream Arrow IPC), intercalados no mesmo canal de stdout apenas quando o protocolo negociado for v2. A negociação ocorre na primeira mensagem (`protocol_version` e lista de capacidades `arrow_batches`). O caminho JSON de `dataset_rows` continua disponível como **fallback explícito** para protocolo v1 e para consultas pequenas, com os limites atuais.
3. **Sem canal de rede.** O IPC não abre porta, socket TCP ou named pipe acessível a outros usuários. Esse é o mecanismo de autenticação local: o pipe é herdado do processo pai e não é exposto. Isso substitui um token compartilhado, que não traria ganho sem canal exposto e exigiria armazenamento de segredo.
4. **Limites mantidos:** `MAX_IPC_BYTES` por frame/linha, `sync_channel` para backpressure, qualquer dado após a mensagem final é erro de protocolo.
5. **Correlação:** cada worker atende uma única requisição de `hostRequest`; por isso `request_id` não é necessário no v1. `session_id` é mantido pelo host (não transita pelo worker). Campos `request_id`/`session_id` mencionados em §4 do plano ficam como campos aditivos a considerar no v2, se houver mais de uma requisição por processo.

Implementação prevista: Arrow em ENT-047 (depende do venv e de `pyarrow` do lockfile) e streaming/backpressure em ENT-031. Esta ADR não implementa Arrow.

### D04 — Gerenciador Python e lockfile

1. **Gerenciador: `uv`** (Astral), versão fixada no manifesto de matriz de versões (ENT-004). Justificativa: resolvedor único e rápido, lockfile com versões completas e hashes, suporte a ambiente isolado (venv) e a índice configurado, instalação em espaço do usuário, sem privilégio, binário único.
2. **Fonte canônica:** `dowai_project.json` (`python.dependencies`, `package_policy`). O `uv.lock` (ou equivalente gerado pelo `uv`) é o lockfile do projeto. `pyproject.toml`, se existir, é gerado ou importado por comando explícito; divergência é reportada por `validate` (ENT-046).
3. **Sem instalação implícita em run produtivo.** Resolver e instalar ocorrem em comando explícito (`dowai env sync` ou equivalente, a definir em ENT-045/ENT-046). Run usa somente o venv com lockfile validado; divergência entre `dowai_project.json`, lockfile e venv bloqueia a execução.
4. **Sem download de interpretador pelo `uv`.** A variável de ambiente `UV_PYTHON_DOWNLOADS=never` é imposta pelo DOWAI ao chamar o `uv`. O interpretador é escolhido pela matriz suportada (ENT-004), conforme §8.1.
5. **Índice autorizado:** o `uv` é configurado com índice explícito (padrão PyPI oficial, sujeito à política do projeto) e modo de verificação de hashes. Índices alternativos exigem configuração administrativa.
6. **Versões:** nenhuma versão de `uv`, `pandas`, `polars`, `pyarrow` ou `sqlalchemy` é homologada por esta ADR. A homologação ocorre em ENT-004 e ENT-046, com registro de comando e resultado em EVIDENCIAS.md.

### D05 — Conexão nativa para Pandas sem URL com segredo

1. **Objeto de capacidade, não string.** `env.connections.get(nome)` (ENT-043) devolve um objeto `Connection` com métodos de capacidade (por exemplo `sqlalchemy_engine()`, `dbapi_connection()`, `read_sql(...)`, `to_sql(...)`), sem atributo `url`, `password` ou `dsn` público.
2. **Segredo não entra em argv, env, log ou auditoria.** O host resolve o perfil de conexão (ENT-030) e entrega o material de conexão ao worker somente pelo canal IPC, uma vez, numa mensagem de abertura de conexão (a ser acrescentada no v2 como operação `connection_open`), com `repr`/`str` mascarados.
3. **Pandas usa o objeto de capacidade:** `pandas.read_sql(query, conn.sqlalchemy_engine())`, sem que o código do modelo manipule a URL.
4. **Não é sandbox.** O código do modelo roda no mesmo processo Python e pode, em tese, acessar objetos em memória. O controle é de exposição acidental e de auditoria, não de isolamento contra código malicioso. Isso é registrado como limitação e tratado no threat model (ENT-011).
5. **Alternativa rejeitada:** proxy de consultas pelo host para evitar o segredo no worker. Inviável para Pandas/SQLAlchemy sem reimplementar o driver e degradaria desempenho.

## Alternativas consideradas

- **D03 — Arrow apenas, sem JSON:** complicaria a depuração e o contrato de controle; rejeitado. JSON segue para controle.
- **D03 — Arrow em base64 dentro do JSON:** aumenta o volume em ~33% e exige cópia; rejeitado.
- **D03 — Socket TCP em loopback com token:** superfície de rede e segredo adicionais sem necessidade; rejeitado.
- **D04 — pip + pip-tools / Poetry / Pixi / Conda:** pip-tools e Poetry exigem orquestração própria de venv e lockfile, sem o mesmo conjunto de recursos; Pixi/Conda trazem gerenciador de ambiente pesado e fora do modelo de venv do projeto; rejeitados como padrão, podendo ser revisitados se a matriz de plataforma (Windows MSI, ENT-093/094) exigir.
- **D04 — Instalar pacotes no momento do run:** viola §8.4 (não instalar automaticamente em run produtivo); rejeitado.
- **D05 — Senha em variável de ambiente do worker:** exposição em `/proc` e em logs de crash; rejeitado.

## Consequências

- O protocolo v1 segue suportado; v2 será adicionado de forma aditiva com negociação explícita. Nenhuma fixture atual deixa de valer.
- A adoção de `uv` cria dependência externa de ferramenta, que precisa de detecção pelo `doctor` (ENT-016), de fixação de versão (ENT-004) e de análise de supply chain (ENT-074).
- Arrow só será disponível quando o venv com `pyarrow` estiver validado (ENT-046/ENT-047).
- O caminho JSON limitado permanece como fallback observável; a ausência de Arrow não pode ser apresentada como suporte a volume grande.

## Divergências registradas (código/contrato × plano)

| # | Ponto | Plano | Estado atual | Tratamento |
|---|---|---|---|---|
| V1 | Transporte de dados | Arrow IPC ou streaming comprovado (§4) | JSON `dataset_rows`, lotes ≤10 000 e 20 MiB | Mantido como fallback; Arrow em v2 (ENT-047). Pendente. |
| V2 | Autenticação local | "autenticação local" (§4) | Pipe herdado, sem token | Decidido: pipe é a fronteira; registrado em D03.3. |
| V3 | Campos `request_id`/`session_id` | Presentes no protocolo (§4) | Ausentes no `ipc.schema.json` v1 | Não necessário no v1 (uma requisição por worker); reavaliar no v2. |
| V4 | Comentário de cabeçalho do runtime | Operações de dataset | Comentário cita só `schema`; código e schema já têm `rows` | Documentação desatualizada; corrigir junto com ENT-041. Pendente. |
| V5 | Gerenciador e lockfile | Resolvedor reproduzível (D04) | Não existe; `validate_python_runtime` rejeita instalação | Decidido (D04); implementação em ENT-045/046. |
| V6 | Conexão nativa | Capability sem URL (D05) | Não existe `env.connections.get` | Decidido (D05); implementação em ENT-043. |

## Testes e validações associados

- Contratos: `cargo test -p dowai-core --test schema_contracts` (valida que `ipc.schema.json` tem fixtures positivas e negativas).
- Runtime IPC: `cargo test -p dowai-runtime` (protocolo, limites, cancelamento, dados em lote).
- SDK: `PYTHONPATH=src python3 -m unittest discover -s tests -p "test_*.py"` em `src/dowai-sdk` (25 testes).

Testes previstos para implementação futura (não executados nesta tarefa): negociação v2 e frames Arrow (ENT-047), `uv lock` com verificação de hashes e divergência (ENT-046), `connection_open` sem vazamento de segredo em logs e argv (ENT-043).
