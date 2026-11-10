# DOWAI — Plano mestre técnico de produto, arquitetura, implementação, testes e distribuição

> **Documento normativo e substitutivo — versão 1.1**  
> Produto: **DOWAI — Data Objects With AI**  
> Data da revisão: **7 de novembro de 2026**  
> Estado: **especificação-alvo aprovada em seus princípios; funcionalidades não presumidas como implementadas**  
> Destinatário: coordenação técnica e execução do projeto  
> Idioma deste documento: português. **Código, nomes públicos de APIs, identificadores, schemas e códigos de erro: inglês.**  


## 0. Natureza, autoridade e uso deste documento

Este documento é a **fonte única de requisitos de produto** para iniciar, implementar, integrar, testar, empacotar e distribuir o DOWAI. Não é um relato de funcionalidades existentes, cronograma fechado, organograma nem promessa de versões de terceiros. Deve ser acompanhado por ADRs, schemas, código, testes, resultados de CI e documentação operacional produzidos durante sua execução.

### 0.1 Convenções normativas

- **DEVE / NÃO DEVE**: requisito obrigatório; **DEVERIA**: recomendação sujeita a justificativa formal; **PODE**: opção.
- **MVP**: núcleo mínimo tecnicamente utilizável e demonstrável; **GA**: primeira distribuição pública suportada para Linux e Windows.
- Nenhum requisito é considerado entregue sem caminho real de execução, teste reproduzível e evidência.
- Estados de implementação: `NOT_IMPLEMENTED`, `PARTIAL`, `IMPLEMENTED`, `VERIFIED`, `RELEASED`. `VERIFIED` exige testes executados; `RELEASED` exige artefato publicado e comprovadamente instalável.
- Mudanças incompatíveis em contrato público exigem ADR, avaliação de migração, atualização de schema, fixtures, testes e changelog.
- As especificações concretas de APIs, drivers, bibliotecas, versões e sistemas operacionais devem ser verificadas nas documentações oficiais no início de sua implementação. **Não fixar versões sem matriz de compatibilidade e testes.**
- Segurança, integridade, idempotência e auditabilidade prevalecem sobre conveniência e desempenho.
- Nenhuma sugestão de IA é executada automaticamente; nenhum segredo é registrado ou encaminhado sem autorização.
- Este plano **não define cargos, quantidade de pessoas, especialidades, atribuições de indivíduos ou estrutura de equipe**. A distribuição do trabalho é baseada em pacotes entregáveis, interfaces, dependências e critérios de aceite.

### 0.2 Escopo e promessa

O DOWAI é um **motor de execução e transformação de dados em Rust** com modelos SQL executados no SGBD e modelos Python executados em ambiente supervisionado. Integra planejamento declarativo, execução manual imperativa, sessões com variáveis, bibliotecas Python por projeto, conectores, plugins globais e locais, SDK tipado, observabilidade e IA opcional via `env.ia`.

**Não é** um scheduler corporativo completo, substituto universal de Airflow/Dagster, clone integral de dbt, serviço cloud multi-tenant, nem sandbox seguro para código Python não confiável sem isolamento adicional.

### 0.3 Decisões aprovadas e invariantes de arquitetura

| ID | Decisão vinculante | Consequência verificável |
|---|---|---|
| P01 | Núcleo em Rust | Workspace Cargo modular e serviço de execução único |
| P02 | SQL é executado no SGBD configurado | Pushdown, controle de conexão e materialização |
| P03 | Python roda em processo supervisionado DOWAI | SDK injetado, IPC versionado, timeout e cancelamento |
| P04 | Modo automático usa DAG de dependências declaradas | Plano estático e ordenação topológica |
| P05 | Modo manual é explícito em CLI e Python | Respeita a ordem fornecida, sem executar predecessores automaticamente |
| P06 | `cancelHierarchy()` **não existe** | Não criar alias, implementação nem documentação desse método |
| P07 | `env.run()` não cria arestas no DAG estático | Registrar invocação no histórico de execução, separado do lineage |
| P08 | CLI e SDK Python utilizam o mesmo motor e políticas | Paridade de semântica, resultados, auditoria e erros |
| P09 | Variáveis são tipadas e compartilhadas na sessão corrente | Templates SQL e Python acessam o mesmo contexto |
| P10 | Templates SQL oferecem condicionais por `var()` | Selecionam trechos/consultas SQL; não são substituição SQL insegura |
| P11 | Erro não tratado interrompe sequência | `try/except` no Python ou política explícita de continuação |
| P12 | Dependências Python são declaradas por projeto | Lockfile, autorização configurável, venv e integridade |
| P13 | JSON é formato nativo DOWAI | Schemas JSON versionados; YAML lido para compatibilidade dbt/legado |
| P14 | Configuração do usuário usa convenções do sistema | XDG no Linux; AppData no Windows; workspace independente |
| P15 | Linux `.deb` e Windows `.msi` são produtos oficiais | Instalação, upgrade, reparo/desinstalação e smoke tests |
| P16 | Plugins globais e locais têm dispatcher único | `env.plugins.run()` e `env.plugins.namespace.method()` equivalentes |
| P17 | `env.ia` é opcional e restrito por política | Produto funciona sem internet/IA; IA não altera código automaticamente |
| P18 | Lineage segue OpenLineage | Eventos/datasets/facets aderentes a schema, com inferências identificadas |
| P19 | Compatibilidade dbt é deliberadamente parcial | Fixtures e diagnóstico explícito para recursos não suportados |
| P20 | Todo limite operacional ajustável tem parâmetro documentado | Defaults, faixas, precedência e limites administrativos definidos |
| P21 | Logs e eventos AUDIT são distintos | Correlation IDs, mascaramento, retenção, rastreabilidade |
| P22 | Incremental é definido pela lógica do usuário | O motor fornece primitivas seguras; não presume regra de negócio |

## 1. Fronteira funcional e marcos

### 1.1 Entregas por estágio

| Estágio | Entrega demonstrável | Gate obrigatório |
|---|---|---|
| M0 — contratos e base | ADRs, schemas, estrutura, CI, threat model inicial, status | Validadores de schema e CI em execução |
| M1 — configuração, CLI e planejamento | `init`, `validate`, `plan`, `doctor` básico, sessões e DAG, planejamento manual | Parse offline e diagnósticos determinísticos |
| M2 — SQL real | Execução em banco, fontes, templates, variáveis, table/view, transações, compatibilidade dbt básica | E2E Postgres/MySQL quando suportado, sem mocks no caminho real |
| M3 — Python/SDK | Runtime Python, IPC, sessão, `Env`, datasets lazy, manual/automático, tratamento de falhas | CLI e Python com comportamento equivalente em E2E |
| M4 — plugins e integração de dados | Dispatcher, typing, stubs, files/rest/email/db, dependências por projeto, escrita em destino | E2E plugin real e transferência controlada entre bancos |
| M5 — IA, semântica e auditoria | `env.ia`, dictionary, OpenLineage, sugestões, AUDIT, retenção | Schemas válidos, segurança e testes de IA desativada |
| M6 — robustez | Recuperação, incremental dirigido por usuário, outbox, retries, streaming, chaos, performance | Replay sem duplicação indevida e benchmark reproduzível |
| M7 — release | MSI, DEB, tarball/zip quando apropriado, documentação, assinatura, matriz e smoke tests | Install/upgrade/uninstall Windows/Linux e release gates completos |

Um estágio só pode ser declarado concluído com artefatos e evidências. Recursos posteriores podem ser desenvolvidos antes, mas não podem comprometer contratos congelados ou simular aceite antecipado.

### 1.2 Fora do MVP

- Scheduler cron, interface web e gestão de clusters distribuídos.
- Marketplace e instalação remota irrestrita de plugins.
- Execução de Python de terceiros com alegação de isolamento total.
- Suporte integral a dbt, hooks/packages/snapshots/seeds, dialetos cloud sem testes.
- Inferência automática de regras incrementais, exclusão de registros sem estratégia explícita.
- Aplicação de correções sugeridas por IA sem revisão.
- Sincronização automática de variáveis de sessão entre máquinas sem serviço explícito.

## 2. Arquitetura e responsabilidades técnicas

### 2.1 Fluxo obrigatório

```text
CLI ────────────┐
               ├──> Application Command API (Rust) ──> Policy/Config/Session
Python SDK ────┘                         │
                                      ├──> Resolver/Parser/Planner (DAG)
                                      ├──> Execution Supervisor
                                      │       ├──> SQL Adapters → DBMS
                                      │       └──> Python Runtime → env/IPC
                                      ├──> Plugin Dispatcher
                                      ├──> State/Audit/Lineage
                                      └──> Optional AI Gateway
```

Não existem planejadores, regras de permissão ou engines de execução separados para CLI e SDK. CLI serializa argumentos para um comando interno; SDK envia o mesmo comando pela fronteira IPC. Saídas humanas da CLI não são contrato de API; saída `--json` e objetos Python usam o mesmo modelo lógico versionado.

### 2.2 Componentes propostos

| Crate/pacote | Contrato mínimo |
|---|---|
| `dowai-cli` | Parse de argumentos, apresentação, exit code, sinais |
| `dowai-config` | Descoberta, merge, schema, defaults, security ceilings |
| `dowai-parser` | AST SQL/templates e análise estática segura de Python |
| `dowai-graph` | DAG, topo sort, ciclos, análise de referências |
| `dowai-session` | Sessão, variáveis, revisões, transações de contexto, TTL |
| `dowai-runtime` | Command API, planos, manual/auto, scheduler, cancelamento |
| `dowai-state` | Run/node/attempt, journal e recuperação |
| `dowai-connectors` | Perfis, SQL, leitura em lotes, escrita, transações |
| `dowai-python` | Venv, subprocessos, IPC, controle de recursos |
| `dowai-plugins` | Resolver, manifesto, políticas, dispatcher |
| `dowai-ai` | Providers, contratos e política de privacidade |
| `dowai-observability` | Logs, AUDIT, métricas, OpenLineage |
| `dowai-sdk` (Python) | `Env`, datasets, resultados e tipagem |

Esses nomes são limites de responsabilidade sugeridos; reorganização física é permitida por ADR sem duplicar lógica.

### 2.3 Contratos internos e concorrência

- Protocolo IPC versionado, com `protocol_version`, `request_id`, `session_id`, operação, argumentos tipados, resultado ou erro normalizado. Aplicar limites de tamanho, timeout, autenticação local e mascaramento.
- Transporte de controle por JSON validado; transporte de dados volumosos por Arrow IPC ou streaming equivalente comprovado. Não transferir tabelas inteiras por JSON.
- Tokio para I/O e supervisão; Rayon apenas para CPU-bound Rust justificado por benchmark. Bibliotecas Python podem usar processos/threads próprios dentro de limites observáveis.
- Identificadores globalmente únicos, timestamps UTC, hashes de scripts/config/plano e pinagem de dependências para reprodução.
- Instalação, resolução de projetos, configs, caminhos e módulos precisa funcionar em Windows e Linux sem pressupor separador `/` ou shell Bash.

## 3. Distribuição de arquivos e configuração

### 3.1 Localização padrão

| Categoria | Linux | Windows |
|---|---|---|
| Binário | `/usr/bin/dowai` | `%ProgramFiles%\DOWAI\dowai.exe` para instalação por máquina |
| Configuração administrativa | `/etc/dowai/config.json` | `%ProgramData%\DOWAI\config.json` |
| Configuração do usuário | `${XDG_CONFIG_HOME:-~/.config}/dowai/config.json` | `%APPDATA%\DOWAI\config.json` |
| Dados/estado do usuário | `${XDG_STATE_HOME:-~/.local/state}/dowai/` | `%LOCALAPPDATA%\DOWAI\state\` |
| Cache | `${XDG_CACHE_HOME:-~/.cache}/dowai/` | `%LOCALAPPDATA%\DOWAI\cache\` |
| Workspace padrão | `${XDG_DATA_HOME:-~/.local/share}/dowai/workspace/` | `%USERPROFILE%\Documents\DOWAI\` |

As variáveis XDG têm precedência sobre seus defaults no Linux; caminhos devem ser normalizados por APIs nativas. `DOWAI_WORKSPACE` e `--workspace` selecionam workspace diferente. Um perfil corporativo pode definir localização distinta sem requerer root/admin para operações cotidianas. Dados do usuário não devem ser removidos automaticamente na desinstalação.

### 3.2 Layout de workspace/projeto

```text
workspace/
├── workspace.json                # perfis, defaults de workspace
├── data_dictionary.json          # semântica global
├── plugins/
│   └── email/plugin.json
└── projects/
    └── fat/
        ├── dowai_project.json   # identidade, política e overrides
        ├── sources.json
        ├── pyproject.toml        # opcional; interoperabilidade Python
        ├── uv.lock               # exemplo de lockfile do resolvedor escolhido
        ├── data_docs/
        │   ├── dictionary.json
        │   └── lineage.json
        ├── plugins/
        ├── models/
        │   ├── faturamento.sql
        │   └── comissao.py
        ├── tests/
        ├── docs/
        ├── .dowai/               # estado/cache/stubs locais; ignorar em Git
        └── .dowai_venv/          # venv; ignorar em Git
```

`pyproject.toml`, lockfiles específicos de ferramentas, `dbt_project.yml` e `sources.yml` são formatos externos com finalidade própria, **não** uma reversão da decisão de usar JSON para configurações nativas DOWAI. Linhagem compatível com OpenLineage usa JSON; eventos operacionais persistidos como JSONL versionado.

### 3.3 Resolução e precedência

1. Parâmetros explícitos da chamada CLI/SDK (quando o campo for substituível).
2. `dowai_project.json`.
3. `workspace.json`.
4. Configuração do usuário.
5. Configuração administrativa do sistema.
6. Defaults versionados do motor.

**Exceção obrigatória:** políticas de segurança e tetos de recursos definidos por administrador **não podem ser ampliados** pelo projeto, workspace, variável de ambiente ou parâmetro CLI. A resolução efetiva aplica interseção de permissões e limites mais restritivos. Erro de violação não é convertido em fallback permissivo.

Chaves desconhecidas falham em `validate --strict`; em modo normal causam diagnóstico e falham quando afetam comportamento crítico. Toda configuração efetiva pode ser inspecionada com `dowai config show --effective --json`, mascarando segredos e indicando origem de cada campo. Definir comportamento para `null`, ausência, arrays (substituição, salvo schema diferente), mapas (merge por chave) e conflitos.

### 3.4 Catálogo de parâmetros configuráveis

**Regra geral:** qualquer opção operacional com variação legítima entre projetos, máquinas ou ambientes deve ter entrada tipada no JSON Schema, default documentado, validação, unidade e precedência. Limitações intrínsecas de segurança podem ser constantes não relaxáveis. Não espalhar números mágicos pelos módulos.

| Grupo | Parâmetros mínimos | Regras |
|---|---|---|
| `runtime` | `max_parallel_nodes`, `max_worker_processes`, `max_threads`, `max_cpu_percent`, `max_memory_mb`, `max_inflight_bytes`, `queue_capacity`, `shutdown_grace_seconds` | Inteiros/limites validados; nunca ultrapassar teto administrativo |
| `execution` | `default_mode`, `node_timeout_seconds`, `run_timeout_seconds`, `max_call_depth`, `max_invocations_per_run`, `stop_on_error`, `retry_max_attempts`, `retry_backoff_ms`, `retry_jitter`, `default_profile`, `allow_manual_mode` | `stop_on_error=true` por padrão; retries apenas seguros |
| `python` | `interpreter`, `venv_path`, `install_tool`, `dependency_lock_required`, `max_threads`, `process_timeout_seconds`, `package_policy`, `allow_unlisted_packages` | Ambiente por projeto e integridade verificada |
| `database` | `pool_max_connections`, `connect_timeout_seconds`, `query_timeout_seconds`, `fetch_batch_rows`, `write_batch_rows`, `max_transfer_bytes`, `tls_required` | Por perfil/adapter com capabilities |
| `sessions` | `ttl_seconds`, `max_variables`, `max_value_bytes`, `max_context_bytes`, `allow_resume`, `persist_state`, `conflict_policy` | Persistência local controlada e limpeza auditável |
| `templates` | `strict_undefined`, `max_render_bytes`, `max_nesting_depth`, `allow_dowai_if_alias`, `allowed_macros` | Apenas subset seguro; sem Python arbitrário |
| `plugins` | `require_manifest`, `allow_local_override`, `max_parallel_calls`, `default_timeout_seconds`, `allow_network`, `allow_filesystem`, `require_idempotency_key` | Permissões granulares por plugin |
| `logging` | `level`, `audit_enabled`, `retention_days`, `rotation_max_mb`, `compression`, `redaction_enabled`, `console_format` | AUDIT essencial não pode ser desativado por projeto |
| `lineage` | `enabled`, `emit_mode`, `output_path`, `namespace`, `fail_on_emit_error` | Esquema OpenLineage validado |
| `ai` | `enabled`, `default_profile`, `max_tokens`, `max_cost_per_run`, `timeout_seconds`, `allow_data_rows`, `redaction` | Opt-in, sem dados brutos por default |
| `security` | `allowed_paths`, `allowed_hosts`, `allowed_profiles`, `allow_external_writes`, `require_approval_for_destructive`, `allow_insecure_tls` | Deny-by-default em operações sensíveis |
| `state` | `backend`, `path`, `busy_timeout_ms`, `checkpoint_policy`, `retention_days` | Recovery e integridade |

Configurações de limites são aplicadas no runtime; registrar quando o sistema operacional não oferece enforcement preciso. A soma de recursos dos processos filhos deve ser controlada até onde a plataforma permitir. Não afirmar que `max_cpu_percent` limita perfeitamente Python/threads em todos os sistemas sem validação.

### 3.5 Exemplo normativo de configuração (fragmento)

```json
{
  "schema_version": 2,
  "workspace_name": "meu_workspace",
  "runtime": {
    "max_parallel_nodes": 4,
    "max_worker_processes": 4,
    "max_threads": 8,
    "max_memory_mb": 2048,
    "max_inflight_bytes": 134217728
  },
  "execution": {
    "default_mode": "automatic",
    "stop_on_error": true,
    "node_timeout_seconds": 3600,
    "retry_max_attempts": 1,
    "allow_manual_mode": true
  },
  "sessions": {
    "ttl_seconds": 86400,
    "max_variables": 1000,
    "max_value_bytes": 65536,
    "allow_resume": true,
    "conflict_policy": "error"
  },
  "templates": {
    "strict_undefined": true,
    "allow_dowai_if_alias": true,
    "max_nesting_depth": 32
  },
  "python": {
    "dependency_lock_required": true,
    "package_policy": "declared_and_authorized",
    "allow_unlisted_packages": false
  },
  "logging": {
    "level": "INFO",
    "audit_enabled": true,
    "retention_days": 90,
    "redaction_enabled": true
  },
  "ai": {
    "enabled": false,
    "allow_data_rows": false,
    "profiles": {}
  }
}
```

Os números apresentados são **defaults iniciais propostos**, ajustáveis conforme schema e plataforma, sujeitos a testes de recursos; não representam SLAs medidos. O schema deve declarar intervalos e cruzar validações (por exemplo, worker processes ≤ teto administrativo). Valores secretos são referências `secret_ref`/`*_env`, nunca literais versionados.

### 3.6 Migração YAML/JSON e compatibilidade

- Novo projeto DOWAI gera somente arquivos de configuração JSON.
- Leitura de `dbt_project.yml`, `sources.yml` e subconjunto dbt é suporte de compatibilidade; manifestos nativos JSON têm precedência explícita.
- Fornecer `dowai config migrate <path> --output <path>.json`, com diff, diagnóstico e sem sobrescrever automaticamente o original.
- Em versões legadas que suportavam `config.yaml`/`plugin.yaml`, leitura pode ser mantida por período de migração documentado, com aviso e política de conflito. Nunca mesclar dois manifestos equivalentes silenciosamente.
- Senhas, tokens e strings secretas não são migrados para arquivos abertos; requerer secret manager ou variável de ambiente.

## 4. Sessões e variáveis

### 4.1 Identificadores e ciclo de vida

- `session_id`: escopo de variáveis e comandos relacionados. Sessão inicia implícita em `dowai run` ou explicitamente em `dowai session start`; termina em `session end`, expiração ou cancelamento administrativo.
- `run_id`: invocação de execução; uma sessão pode conter vários runs sequenciais.
- `invocation_id`: ocorrência específica de `env.run()`/CLI, inclusive quando modelo é chamado repetidamente.
- `node_id`: identidade lógica de modelo dentro de um plano automático.
- `attempt_id`: tentativa de execução de uma invocação/nó; retries não substituem registros anteriores.
- `parent_invocation_id`: vínculo de chamada aninhada, **sem criar aresta no DAG**.

Cada nova invocação CLI isolada cria nova sessão por padrão. Para comandos CLI sucessivos compartilharem variáveis, o usuário DEVE passar `--session <id>` depois de `dowai session start`. Sessões retomáveis persistem contexto em storage local com permissões de usuário e TTL configurável; não podem ser anexadas por outro usuário sem autorização. Scripts Python executados por `env.run()` herdam a sessão corrente. Múltiplos processos ligados à mesma sessão são serializados ou submetidos a controle de concorrência documentado.

### 4.2 Tipos e escopo

Tipos públicos aceitos: `null`, boolean, número finito, string, array e objeto JSON, com limites configuráveis de tamanho e profundidade. Representação explícita de datas/horários em ISO-8601 via strings e helpers tipados; não aceitar objetos Python arbitrários, pickles ou conexões como variável de sessão. Segredos são referências opacas, nunca valores exportados ao template ou log.

API mínima:

```python
env.setVar("carga_total", True)
valor = env.getVar("carga_total", default=False)
env.hasVar("carga_total")
env.unsetVar("carga_total")
env.vars.snapshot()  # somente valores não secretos, cópia imutável
```

As variáveis declaradas por `--vars`, `def_var()` ou `setVar()` permanecem disponíveis a chamadas posteriores **na mesma sessão**. Não são automaticamente globais, permanentes ou compartilhadas entre usuários/projetos. O fechamento/expiração remove o estado recuperável segundo política de retenção/auditoria; auditoria não guarda valores sensíveis.

### 4.3 SQL e templates condicionais

As variáveis **controlam o template SQL antes do envio ao banco**, permitindo `if`, `else`, `elif`, seleção de consultas, fragmentos `WHERE` e materialização declarada. Não representam variáveis de sessão nativas do SGBD.

Sintaxe canônica compatível com Jinja do subconjunto suportado:

```sql
{{ def_var('carga_total', true) }}

{% if var('carga_total') == true %}
    SELECT * FROM vendas;
{% else %}
    SELECT * FROM vendas WHERE atualizado_em >= :data_corte;
{% endif %}
```

Para acolher a notação desejada, o parser DOWAI DEVE reconhecer, como extensão documentada, `{{ if var('carga_total')==true }}`, `{{ else }}`, `{{ elif ... }}` e `{{ endif }}` como equivalentes aos blocos canônicos **apenas nos lugares e formatos previstos**. Não confundir com expressões interpoladas normais `{{ ... }}`. Testar aninhamento, espaços, comentários e fim de arquivo. Recomendar a notação canônica ao importar dbt.

Semântica das macros:

| Macro | Efeito | Contrato |
|---|---|---|
| `def_var('name', default)` | Define padrão **somente se ausente** | Retorna string vazia ao template; grava no contexto após validação |
| `var('name')` | Lê variável existente | Erro se ausente com `strict_undefined`; alternativa `var('name', default)` |
| `if/elif/else/endif` | Escolhe trecho a renderizar | Sem efeitos/SQL executados nas ramificações não escolhidas |
| `ref('model')` | Resolve relação e declara dependência estática | Output SQL escapado por adapter |
| `source('s','t')` | Resolve fonte externa declarada | Não cria nó executável por si só |
| `config(...)` | Configura modelo com subconjunto validado | Não cria dependência por si só |

`def_var` também pode aceitar identificador sem aspas (`def_var(carga_total, true)`) exclusivamente pela gramática DOWAI, com resolução estática inequívoca. Literal `true`/`false` é boolean (não string); Python usa `True`/`False`. Uma variável criada por `def_var` já é visível às expressões subsequentes **do mesmo render**, embora só seja confirmada na sessão após o sucesso do modelo. Variáveis usadas como **valores SQL** devem ser passadas como parâmetros tipados/bind parameters por adapter, nunca interpoladas sem escape; identificadores SQL dinâmicos passam por whitelist/citação específica do dialeto. Não aceitar filtros/expressões Jinja arbitrários ou avaliação livre de Python.

O planejamento estático escaneia `ref()` de todas as ramificações possíveis para obter união conservadora das dependências; uma condição dinâmica não pode introduzir referência de modelo desconhecida após o plano ser aprovado. Renderização reavalia variáveis no início da execução do nó, usando snapshot consistente. O output efetivo e hash são registrados sem segredos.

### 4.4 Concorrência, commit e falhas do contexto

- Cada invocação recebe snapshot versionado de variáveis mais o **overlay de alterações pendentes dos ancestrais**. Escritas produzem revisões com `invocation_id` e origem; conflito de escrita concorrente para a mesma chave é erro por default (`sessions.conflict_policy="error"`).
- `env.setVar()` no modelo chamador é visível ao próximo `env.run()` filho, **mesmo antes de o chamador terminar**. O filho lê o overlay ancestral; suas próprias alterações bem-sucedidas são mescladas de volta ao overlay do chamador, preservando a ordem sequencial.
- Alterações pendentes da árvore de invocações são publicadas na sessão **somente após término bem-sucedido da invocação raiz**. Falha/timeout/cancelamento não tratado descarta o overlay ainda não confirmado; alterações confirmadas por runs anteriores permanecem. Um erro filho tratado por `try` descarta o overlay desse filho, mas permite prosseguir com o estado anterior.
- Banco de dados, arquivos, e-mails e outros efeitos externos **não** participam dessa transação de variáveis: podem já estar confirmados quando o chamador falha. Isso deve ser explicitado e testado. Não alegar rollback distribuído.
- Em fluxos paralelos, snapshots não garantem ordem de atualização: conflitos devem ser detectados e reportados. Não usar last-writer-wins silencioso.
- `plan` pode receber `--vars`, mas NÃO deve gravar o estado da sessão, executar `def_var` com efeitos persistentes ou resolver segredos na saída.

## 5. Modos de execução e DAG

### 5.1 Modo automático (default)

`dowai run faturamento.sql` / `env.run("faturamento.sql")` resolvem o DAG, validam ciclos/políticas, executam predecessores requeridos e, por último, o alvo. Dependências entram no grafo somente por `ref`, `env.ref`, `env.depends_on` e declarações estáticas formalmente reconhecidas. Mesmo arquivo/modelo com IDs duplicados é erro. DAG com ciclo falha antes de efeitos externos.

### 5.2 Modo manual explícito

CLI:

```bash
dowai run --manual arq1.sql arq10.sql arq7.py
dowai plan --manual arq1.sql arq10.sql arq7.py
```

Python:

```python
def model(env):
    with env.manual():
        env.run("arq1.sql")
        env.run("arq10.sql")
        env.run("arq7.py")

    # Alternativa equivalente para uma chamada:
    env.run("arq7.py", mode="manual")
```

**`env.cancelHierarchy()` foi cancelado e NÃO DEVE ser criado.** Os modos são `automatic` e `manual`; `execution.default_mode` é configurável. A opção explícita `--manual`/`mode="manual"` prevalece sobre o default desde que permitida por política. O contexto `with env.manual()` restaura o modo anterior ao sair mesmo em exceção; chamadas internas herdam o modo enquanto o bloco estiver ativo, salvo override explícito autorizado.

No modo manual:

- Executar os alvos na ordem de argumentos/chamadas, sequencialmente por default, sem inserção automática de predecessores.
- A existência de `ref()` continua servindo para resolver relações e documentar linhagem, mas **não dispara cargas predecessor**. Se a relação requerida não existir, falhar com erro claro; não simular sucesso.
- `depends_on()` permanece declaração de dependência estática; a execução manual pode ignorar sua ordem, mas o plano/diagnóstico registra o risco.
- Validar caminhos, extensões, perfis, permissões, conexões, SQL, limites e segurança normalmente.
- `plan --manual` lista a ordem pretendida, aponta dependências ignoradas e não executa efeitos.
- Toda invocação manual explícita executa novamente por default, mesmo se o arquivo já rodou na sessão. Reuso de resultado é apenas por parâmetro futuro explícito e testado.
- Ordem manual não implica transação global entre bancos, arquivos ou plugins.

### 5.3 Semântica de `env.run` e chamadas aninhadas

- Uma chamada `env.run("x.py")` dentro de outro Python **não acrescenta aresta ao DAG estático**. O histórico de invocações registra a relação `parent_invocation_id` e `call_type="imperative"`.
- Em modo automático, o arquivo chamado segue **suas próprias dependências**; em modo manual, essas dependências não são carregadas automaticamente.
- O grafo de dados e o grafo de chamadas são entidades distintas; lineage de `ref` não deve ser confundido com execução observada.
- `env.run()` deve poder chamar `.sql` e `.py`, compartilhar sessão, variáveis, permissões e recursos, e retornar `RunResult` tipado.
- Detectar recursão/ciclos imperativos dinâmicos com pilha de invocações e `execution.max_call_depth` configurável; falhar com cadeia legível antes de loop infinito. Limites de invocações e run timeout aplicam-se a chamadas aninhadas.
- `env.run()` pode chamar alvo ainda não declarado no DAG, sujeito a resolução de caminhos e políticas. 
- Importar arquivo Python para análise do DAG não executa código nem side effects; `env.run` é identificado em AST para avisos, nunca tratado como dependência estática.

### 5.4 Falha, exceções e continuidade

Default obrigatório: `execution.stop_on_error=true`. Em CLI sequencial, a primeira falha interrompe as próximas invocações, retorna exit code não zero e registra as não iniciadas como `SKIPPED` quando fizerem parte do plano. Em Python, `env.run()` levanta `DowaiExecutionError`/subclasse, permitindo tratamento por `try/except`:

```python
def model(env):
    with env.manual():
        env.run("a.sql")
        try:
            env.run("b.sql")
        except DowaiExecutionError as exc:
            env.log.warn("b.sql falhou; o modelo optou por continuar", error_code=exc.code)
        env.run("c.sql")
```

O exemplo requer `from dowai import DowaiExecutionError`. Também pode existir `raise_on_error=False` com resultado explícito `FAILED`; o SDK nunca marca essa invocação como bem-sucedida. Se a exceção for capturada, o chamador pode continuar e seu próprio status final depende de seu resultado; falhas filhas permanecem no journal e auditoria. Política CLI de continuação, se disponibilizada, deve ser parâmetro explícito `--continue-on-error` restrito e auditar a opção. Nunca permitir que isso contorne operações negadas por segurança.

### 5.5 Estados e agendamento

- `RunStatus`: `CREATED`, `VALIDATING`, `PLANNED`, `RUNNING`, `SUCCEEDED`, `FAILED`, `CANCELLED`.
- `InvocationStatus`/`NodeStatus`: `PENDING`, `READY`, `RUNNING`, `SUCCEEDED`, `FAILED`, `SKIPPED`, `CANCELLED`.
- Persistir transições atômicas, timestamps UTC, duração, tentativa, hashes, origem manual/automática e erro normalizado.
- Retries criam novos `attempt_id`; só são automáticos para falhas transitórias com operação comprovadamente idempotente e política explícita.
- Concorrência automática por limites de recursos; modo manual é sequencial por contrato, salvo futura API explícita de paralelismo com semântica de contexto definida.
- `Ctrl+C`/SIGTERM/cancelamento IPC sinaliza processos filhos, drena ou interrompe recursos segundo timeout e registra estado final, sem deixar operações parcialmente confirmadas marcadas como sucesso.

## 6. CLI, API Python e modelo de resultados

### 6.1 CLI público

```text
dowai --version
dowai init <project_name> [--workspace PATH]
dowai doctor [--json] [--check-connections]
dowai config show --effective [--json]
dowai config migrate <path> --output <path>.json
dowai validate [PATH] [--strict] [--json]
dowai plan <file_or_model> [--vars JSON] [--json]
dowai plan --manual <file_or_model>... [--vars JSON] [--json]
dowai run <file_or_model|all> [--profile NAME] [--vars JSON]
dowai run --manual <file_or_model>... [--session ID] [--vars JSON]
dowai run <file_or_model> --full-refresh [--yes]
dowai session start [--project-dir PATH] [--json]
dowai session show <session_id> [--json]
dowai session end <session_id>
dowai runs list [--limit N] [--json]
dowai runs show <run_id> [--json]
dowai logs <run_id> [--node NODE] [--level LEVEL]
dowai retry <run_id> [--failed-only]
dowai plugins list [--scope project|global|all]
dowai plugins info <id>
dowai plugins sync
dowai compatibility check <project_path> [--json]
dowai ia generateLineage <file_or_model>
dowai ia generateHumanDocs <file_or_model>
dowai ai review <run_id>
dowai benchmark <file_or_model>
```

CLI aceita apenas `.sql`, `.py`, `all` ou ID inequívoco de modelo. Caminhos relativos/absolutos são confinados à raiz autorizada, com checagem de symlinks e path traversal. Flags reservadas só aparecem no `--help` como funcionais quando implementadas; opcionalmente retornar `NOT_IMPLEMENTED` de forma explícita.

### 6.2 SDK Python e paridade

```python
from dowai import Env, DowaiExecutionError

def model(env: Env):
    env.setVar("carga_total", True)
    plan = env.plan("faturamento.sql")
    status = env.validate()
    result = env.run("faturamento.sql")
    with env.manual():
        env.run("a.sql")
        env.run("b.py")
    return result
```

`Env` só é instanciado pelo runtime para a execução real; `from dowai import Env` oferece tipagem e mocks para testes, sem permitir contornar autenticação/permissões. Um comando CLI e seu equivalente SDK são mapeados à **mesma Application Command API**, com contrato de erro e resultado equivalente. SDK pode oferecer operações adicionais de nível de objeto (datasets, vars), sem duplicar lógica do engine. Métodos de inspeção que não tenham equivalente CLI devem ser deliberadamente listados em `docs/API.md`.

### 6.3 Contrato de retorno

```json
{
  "schema_version": 2,
  "session_id": "uuid",
  "run_id": "uuid",
  "invocation_id": "uuid",
  "status": "SUCCEEDED",
  "execution_mode": "manual",
  "duration_ms": 1250,
  "nodes": [],
  "warnings": [],
  "error_code": null,
  "message": "Completed"
}
```

Campos não disponíveis ficam ausentes ou `null` segundo schema, nunca assumidos. CLI usa `--json` para automação Airflow/Dagster; stdout contém apenas payload JSON nesse modo e logs vão para stderr ou arquivo. Objetos Python fornecem `status`, `failed`, `run_id`, `duration_ms` e detalhes tipados conforme schema.

### 6.4 Exit codes

| Código | Significado |
|---|---|
| `0` | Sucesso |
| `2` | Configuração/parser/plano inválido |
| `3` | Falha de execução de modelo |
| `4` | Falha de conexão externa |
| `5` | Política/permissão/segurança negada |
| `6` | Não implementado/indisponível |
| `130` | Cancelamento por usuário |

Erro público inclui código estável (`DOWAI_*`), mensagem, dica, caminho/linha quando possível, IDs e causa saneada; não incluir segredos nem stack trace irrestrito no modo normal.

## 7. Modelos SQL, templates e compatibilidade dbt

### 7.1 Pipeline SQL

1. Resolver projeto/perfil/sessão e configuração efetiva.
2. Ler modelo e referências estáticas, validar DAG em modo automático.
3. Obter snapshot de variáveis do início da invocação, renderizar template autorizado.
4. Validar SQL materializado, escolher adapter/dialeto, preparar parâmetros seguros.
5. Executar **no SGBD**; não carregar linhas em Rust por default.
6. Persistir materialização conforme `config`, commit e metadados.
7. Publicar variáveis pendentes somente após sucesso, auditoria, lineage e resultado.

Suportar `table` e `view` no ciclo inicial; `incremental` exige contrato explícito posterior. `source()` declara fontes não executáveis; `ref()` representa modelos e resolve nomes devidamente escapados. `full-refresh` exige autorização, política e confirmação, preferindo staging/swap a `DROP` abrupto. Mudança automática de schema não é padrão.

### 7.2 Subconjunto dbt e diagnóstico

| Recurso | Escopo inicial |
|---|---|
| `ref('x')`, `source('s','t')`, `config(materialized='table'|'view')` | Compatível por fixtures |
| `dbt_project.yml`, `sources.yml` | Leitura parcial declarada |
| `var()`/condicionais | Subconjunto documentado; extensão DOWAI para `def_var` e aliases |
| Incremental com `unique_key` | Só quando teste transacional e semântica definidos |
| Packages, macros arbitrárias, hooks, snapshots, seeds | Não suportados no MVP |
| Dialetos/drivers não testados | `UNSUPPORTED`, sem fallback silencioso |

`dowai compatibility check` produz `SUPPORTED`, `PARTIAL`, `UNSUPPORTED`, localização e sugestão; quando possível, comparar fixtures e resultados com dbt de versão fixada na matriz. Não prometer compatibilidade integral. Templates são renderizados por parser allowlist; nunca executar chamadas Python/Jinja arbitrárias.

## 8. Modelos Python, datasets e interoperabilidade de dados

### 8.1 Runtime

- Entrypoint normativo: `def model(env: Env)`, com retorno `Dataset`, relação/materialização explícita ou `None` se permitido pela configuração do modelo.
- Executar em venv do projeto, por subprocesso supervisionado e IPC; seleção de interpretador por matriz suportada, sem presumir que Python acompanha todos os MSI/DEB.
- Importação feita pelo analisador de DAG é **AST sem execução**; imports/efeitos reais só ocorrem no subprocesso autorizado.
- Encerramento por timeout/cancelamento deve terminar árvore de processos até o limite suportado pela plataforma; registrar limitações.
- `env.depends_on()` indica dependência estática; `env.run()` é imperativo e não altera DAG.

### 8.2 API lazy/datasets

`env.ref(name)` em Python devolve dataset/relação lazy com schema, metadados, filtros, projeções e batches; não obriga transferência integral. Métodos mínimos: `schema`, `select`, `filter`, `iter_batches`, `iter_rows`, `to_arrow`, `to_pandas` e `to_polars`, com limitações de memória explicitadas. Lazy objects não permanecem válidos além do processo sem materialização/persistência. O adaptador tenta pushdown de filtros/projeções seguro; fallback deve ser explícito e observável.

### 8.3 Escrita em outros bancos

API controlada preferencial:

```python
def model(env):
    df = env.ref("faturamento").to_pandas()
    df["comissao"] = df["valor"] * 0.05
    env.write(
        df,
        target="postgres_financeiro",
        table="comissoes",
        mode="append"
    )
```

`env.write()` valida perfil de destino autorizado, existência/schema, modo `append|replace|upsert` conforme capability e política, transações, contagem, batch, timeout, retries e idempotência. `replace` e `upsert` exigem pré-condições; operações destrutivas não são inferidas. Tipos Arrow/Pandas/Polars precisam de mapeamento explícito e testes de timezone, null, decimal, encoding e arredondamento.

Também é permitida integração direta com `pandas.to_sql()`, SQLAlchemy e conectores instalados/autorizados, **sem prometer auditoria transacional completa** fora de `env.write()`. Expor conexão/credencial apenas por interface de capacidade autorizada; preferir `env.connections.get(...)` a URL que possa vazar senha. Registrar que efeitos diretos da biblioteca podem escapar de métricas por linha do DOWAI.

### 8.4 Bibliotecas Python por projeto

O projeto declara bibliotecas e constraints em `dowai_project.json`; resolução e pinagem ficam no lockfile do gerenciador escolhido. Política configurável: `declared_and_authorized` (default), `declared_only` ou `admin_allowlist`, sujeita a interseção com política administrativa. Exemplo:

```json
{
  "schema_version": 2,
  "name": "fat",
  "python": {
    "dependencies": [
      "pandas>=2",
      "polars>=1",
      "pyarrow",
      "sqlalchemy"
    ],
    "package_policy": "declared_and_authorized",
    "allow_unlisted_packages": false
  }
}
```

Faixas acima são exemplos de sintaxe, **não versões homologadas**. Lockfile deve fixar versões completas e, quando suportado, hashes; instalação usa índice autorizado e valida origem. Bibliotecas candidatas à compatibilidade inicial: Pandas, Polars, PyArrow, SQLAlchemy. Dependências transitivas e imports dinâmicos são reconhecidos pela resolução do ambiente e pela política; uma allowlist de pacotes **não é sandbox**. Validar licenças, CVEs e supply chain; não buscar/instalar pacotes automaticamente durante run produtivo sem autorização.

Quando `pyproject.toml` também declarar dependências, definir **uma única fonte canônica de verdade**: no projeto DOWAI nativo, `dowai_project.json` prevalece; `pyproject.toml` pode ser gerado/atualizado pela ferramenta ou importado por comando explícito, com divergência sinalizada por `validate`. Não mesclar listas silenciosamente nem instalar pacotes divergentes do lockfile.

### 8.5 Exportações

`env.generateCsv(data, path, **options)` usa streaming, UTF-8, schema, delimitador, escape, timezone, limites e destino autorizado. Plugin oficial `files` suporta CSV/JSON/Parquet e SFTP com idempotência/outbox quando necessário. Nomes/paths devem ser validados contra traversal/symlink.

## 9. Plugins Python, extensibilidade e IntelliSense

### 9.1 Interfaces e dispatcher

```python
env.plugins.run("email.send", to=["ops@example.org"], subject="Alerta", message="...")
env.plugins.email.send(to=["ops@example.org"], subject="Alerta", message="...")
```

As duas APIs **DEVEM** usar exatamente o mesmo dispatcher, manifesto resolvido, checagem de permissões, validação de assinatura, timeout, idempotência e trilha AUDIT. Não usar implementação independente para o atalho tipado. `env.plugins.run` localiza `namespace.method` de um plugin de `/plugins` de projeto ou workspace/global.

### 9.2 Exemplo de manifesto JSON

```json
{
  "schema_version": 2,
  "id": "email",
  "version": "1.0.0",
  "entrypoints": { "send": "sender:send" },
  "permissions": {
    "network": ["mail_gateway"],
    "filesystem": [],
    "secrets": ["mail_credentials"]
  },
  "timeout_seconds": 30,
  "side_effects": true,
  "idempotency_key_required": true
}
```

Manifestos são validados por JSON Schema. Plugin local do projeto com mesmo ID substitui **integralmente** o global; não mesclar métodos. Duplicidade no mesmo nível é erro. Registrar caminho, versão, hash e origem. Manifesto/config pode restringir permissões do plugin; permissões não declaradas são negadas.

### 9.3 Execução, limites e segurança

- Plugins em subprocesso supervisionado com venv do projeto ou ambiente de plugin definido pela política; timeout e recursos configuráveis.
- Descoberta por manifestos e análise estática; não importar/rodar plugins para extrair assinaturas.
- Permissões de rede, filesystem, banco e segredos são granularmente autorizadas. Venv não substitui sandbox.
- Efeitos externos exigem chave de idempotência e, quando apropriado, outbox durável. Falha após commit de banco e antes da entrega deve ser recuperável, mas não é transação distribuída automática.
- Logs de plugins são saneados; stdout/stderr capturados com tamanho máximo configurável.
- Execução remota de código, instalações dinâmicas e plugins sem manifesto não são habilitados por default.

### 9.4 SDK tipado e stubs

Pacote `dowai-sdk` separado do motor, instalável via gerenciador Python do projeto. `dowai plugins sync` gera `.pyi` por projeto, determinístico e atomicamente, refletindo overrides e assinaturas; parâmetros sem anotação usam `Any` com aviso. Integração Pylance/Pyright verificada em fixture real. `sync` não executa código; `watch`/extensão VS Code são evoluções.

### 9.5 Plugins oficiais mínimos e testes

| ID | Capacidades mínimas | Fixture de aceite |
|---|---|---|
| `rest` | GET/POST, auth por segredo, paginação, 429/retry/rate limit | API falsa paginada e timeout |
| `email` | SMTP/API, templates, anexos, lote | Gateway mock e deduplicação |
| `files` | CSV/JSON/Parquet/SFTP | Round-trip e arquivo maior que RAM |
| `db` | Leitura, escrita, transferência controlada | Postgres→MySQL e rollback |
| `alerts` | Limiar, destinatários e outbox | Fronteiras 10%/15%, retries seguros |

## 10. Conectores SQL, transações e incremental

### 10.1 Escopo dos adapters

Primeiro conjunto de suporte a validar: PostgreSQL como origem; PostgreSQL e MySQL como destinos, incluindo testes de escrita/leitura e cross-database. Oracle e outros drivers dependem de ADR, demanda, licença, disponibilidade multiplataforma e testes. Cada adapter declara capabilities: DDL transacional, quoting, schemas, staging, bulk load, stream, merge/upsert, TLS, parâmetros, contagem de linhas e cancelamento.

Interface interna conceitual:

```text
test_connection(profile) -> Capabilities
introspect_table(name) -> Schema
execute_sql(sql, parameters, transaction?) -> Result
stream_query(sql, parameters, batch_size) -> BatchStream
begin_transaction() -> Transaction
write_batch(transaction, target, batch) -> Counters
merge(transaction, staging, target, keys) -> Counters
commit(transaction) | rollback(transaction)
```

### 10.2 Regras operacionais

- O banco é responsável por computação SQL sempre que possível. Extração/streaming apenas quando explicitamente solicitado por Python/exportação/transferência.
- Conexões e timeouts por perfil, TLS configurável com baseline seguro, pooling limitado e credenciais por referência.
- SQL parametrizado para valores; identificadores são escapados segundo o dialeto.
- Escrever entre bancos implica transações **separadas**; não anunciar atomicidade entre origem e destino. Staging, idempotency keys, checkpoints e compensação são definidos por workflow.
- `mode="replace"` e `full-refresh` exigem política/confirmação e preferem staging/swap. Schema mismatch falha por padrão.
- Contagens desconhecidas são marcadas como `unknown`, nunca inventadas. Backpressure obrigatório para streaming.

### 10.3 Incremental dirigido pelo usuário

O DOWAI **não inventa** regra incremental: usuário define filtro, chave, watermark, exclusões, UPSERT e condição de confirmação. Runtime fornece contratos de transação, checkpoint persistente, lock e replay. `checkpoint` só avança após commit confirmado; falha pré-commit preserva estado anterior. Estratégias CDC/tombstone são opcionais e não prometidas sem suporte. Testes exigem segunda execução sem duplicação indevida para o fluxo de exemplo, mas não generalizam essa propriedade a SQL arbitrário do usuário.

## 11. IA, dicionário semântico e OpenLineage

### 11.1 IA opt-in

`env.ia` usa gateway com provider/model/profile, credencial por referência, timeout, custo/tokens e política de dados configuráveis. Sem IA, núcleo SQL/Python/plugins continua funcional. Operações planejadas:

```python
env.ia.generateLineage("models/comissao.py")
env.ia.generateHumanDocs("models/comissao.py")
env.ia.suggestSqlFix(error_context)
```

Linhas de dados e segredos não são enviados por default. Redação de SQL/errors antes de chamadas externas. Respostas de IA são tratadas como sugestões não confiáveis; nenhuma alteração de código, SQL, configuração ou execução ocorre sem revisão/autorização explícita. Falha de IA não pode corromper estado de execução.

### 11.2 Dicionários semânticos JSON

```json
{
  "schema_version": 2,
  "metrics": {
    "faturamento_liquido": {
      "description": "Faturamento excluindo impostos em vendas faturadas",
      "synonyms": ["receita_liquida", "vendas_liquidas"],
      "formula": "SUM(tab_vendas.valor_fat) - SUM(tab_vendas_fiscal.valor_impostos)",
      "source_tables": ["tab_vendas", "tab_vendas_fiscal"],
      "grain": ["id_venda"],
      "filters": ["tab_vendas.status = 'FATURADA'"],
      "notes": "Preservar uma linha por venda antes da agregação"
    }
  }
}
```

Dicionário global e local são mesclados por ID completo; local substitui global e origem é auditável. Validar schema offline e colunas/relações com banco quando permitido. Fórmulas não são executadas indiscriminadamente; IA não fabrica regra de negócio e grava em dicionário sem revisão. Recursos futuros: dimensões, chaves, relações, exemplos e tradução com ADR/schema.

### 11.3 OpenLineage e histórico

- Dados de lineage determinísticos vêm de `ref`/`source`, SQL/AST, perfis, entradas/saídas confirmadas, não de mera proximidade temporal de execuções.
- Eventos compatíveis com OpenLineage são estruturados como Run/Job/Dataset, com facets versionadas e validação contra schema oficial fixado em teste. Facets customizadas DOWAI devem ter namespace próprio.
- Registrar separadamente **dependência declarada**, **execução observada** e **inferência de IA**, indicando método, confiança e evidência.
- `lineage.json` pode conter exportação ou eventos conformes ao padrão, conforme `lineage.emit_mode`; não chamar JSON arbitrário de OpenLineage.
- Evitar consultas, tokens ou dados sensíveis em facets/exportações. Emissão externa depende de política e configuração de destino.

### 11.4 AutoFix

Em erro SQL elegível: rollback → AUDIT → contexto saneado → IA autorizada → sugestão versionada `*.suggested.sql` com diff, explicação e testes sugeridos → execução permanece `FAILED`. Falhas de rede, credencial e constraint não são tratadas como sintaxe corrigível. Nenhuma sugestão é aplicada ou executada automaticamente.

## 12. Estado, observabilidade e política de segurança

### 12.1 Persistência e recuperabilidade

Implementar store local transacional (opção inicial como SQLite, sujeito a ADR) para sessões, revisões de variáveis, runs, invocações, tentativas, outbox, checkpoints e journal. Separar caches descartáveis de estado durável e dados versionáveis. Operações críticas devem tolerar encerramento entre preparação, envio, commit e registro; recovery identifica estados incertos e evita marcar sucesso não confirmado. Definir migração de schema de estado, backup, retenção e restauração testada.

### 12.2 Logs e AUDIT

- Níveis operacionais `ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`. `AUDIT` é fluxo distinto, persistido independentemente de nível visual.
- Campos mínimos: `session_id`, `run_id`, `invocation_id`, `node_id`, `attempt_id`, `actor`, UTC, durações, engine/version, script/config/plan hashes, perfis de origem/destino, modo manual/auto, quantidades conhecidas, memória de pico mensurável, resultado, erro e artefatos.
- JSONL rotacionado, compressão, retenção, permissões e redaction configuráveis; logs locais não são imutáveis apenas por possuir hash. WORM/assinatura para necessidade empresarial futura.
- Em manual mode, registrar warning/consentimento quando ignorar dependências; não elevar o risco operacional a privilégio de segurança.
- Histórico permite correlacionar invocações aninhadas mesmo quando não há dependência estática.

### 12.3 Segurança mínima

1. Princípio do menor privilégio para banco, rede, segredos, filesystem, plugins e políticas.
2. `secret_ref`, variáveis ou secret manager para credenciais; nada real em Git, erro, log, prompt, plano JSON ou lineage.
3. Verificação de paths canônicos, symlinks e fronteira do projeto/workspace; recusar escapes sem allowlist administrativa.
4. Autorizações explícitas para escrita externa, operação destrutiva, acesso a rede e pacotes; política administrativa não pode ser afrouxada no projeto.
5. Dependências Python fixadas/validadas, verificação de supply chain, licenças e vulnerabilidades.
6. Cancelamento/timeout com limpeza de processos e arquivos temporários; sem afirmar sandbox completo sem testes específicos por SO.
7. Testes contra SQL injection no template, vazamento de segredos, traversal, arquivos alterados entre validação/execução, desvio de perfis e logs sensíveis.
8. Documentar limites da segurança de código Python arbitrário executado pelo próprio usuário: um venv não impede acesso ao sistema sem sandbox de SO.
9. Ações sensíveis em AUDIT mesmo quando o usuário captura exceção.

## 13. Organização do código e artefatos de engenharia

Estrutura recomendada, criável incrementalmente quando houver conteúdo útil:

```text
dowai/
├── README.md
├── LICENSE
├── CONTRIBUTING.md
├── SECURITY.md
├── CHANGELOG.md
├── Cargo.toml                 # workspace Rust
├── Cargo.lock
├── .github/workflows/
├── src/                       # todo o código-fonte
│   ├── dowai-cli/             # existente
│   ├── dowai-core/            # existente; divisão em crates abaixo por ADR
│   ├── dowai-postgres/        # existente
│   ├── dowai-runtime/         # existente
│   ├── dowai-sdk/             # existente; pacote Python
│   ├── dowai-config/          # planejado (W02)
│   ├── dowai-parser/          # planejado (W04)
│   ├── dowai-graph/           # planejado (W04)
│   ├── dowai-session/         # planejado (W05)
│   ├── dowai-state/           # planejado (W12)
│   ├── dowai-connectors/      # planejado (W06)
│   ├── dowai-python/          # planejado (W08)
│   ├── dowai-plugins/         # planejado (W10)
│   ├── dowai-ai/              # planejado (W11)
│   ├── dowai-observability/   # planejado (W12)
│   └── official-plugins/      # planejado (W10)
├── specs/
│   ├── config.schema.json
│   ├── project.schema.json
│   ├── plugin.schema.json
│   ├── data_dictionary.schema.json
│   ├── plan.schema.json
│   ├── session.schema.json
│   ├── commands.schema.json
│   ├── events.schema.json
│   └── ipc.schema.json
├── examples/
│   ├── sql-basic/
│   ├── sql-conditional-vars/
│   ├── automatic-dag/
│   ├── manual-sequence/
│   ├── python-pandas-cross-db/
│   ├── local-plugin-override/
│   ├── rest-pagination/
│   └── purchase-chain/
├── tests/
│   ├── fixtures/
│   ├── contracts/
│   ├── integration/
│   ├── e2e/
│   ├── chaos/
│   └── security/
├── docs/
│   ├── ADR/
│   ├── STATUS.md
│   ├── ROADMAP.md
│   ├── COMPATIBILITY.md
│   ├── API.md
│   ├── CONFIGURATION.md
│   ├── OPERATIONS.md
│   ├── SECURITY.md
│   └── PERFORMANCE.md
└── packaging/
    ├── debian/
    └── windows/
```

Manter contratos e fixtures sob versionamento; não criar interface falsa somente para preencher estrutura. O nome exato de cada crate pode ser adaptado por ADR sem desrespeitar fronteiras. Nenhum binário/venv/cache de desenvolvimento deve ser commitado inadvertidamente.

## 14. Plano verificável de implementação

Os pacotes abaixo representam **trabalho entregável**, não papéis profissionais. Cada pacote deve ser subdividido em mudanças pequenas, com ownership de arquivos definido no plano de integração, contratos estáveis e testes. Não distribuir implementação concorrente de um mesmo contrato indefinido.

| Pacote | Entradas/contratos | Saída obrigatória | Dependência |
|---|---|---|---|
| W00 Governança técnica | Este plano, ADR template | Status, CI, matriz de decisão, Definition of Done | Nenhuma |
| W01 Schemas públicos | P01–P22 | JSON Schemas, fixtures positivas/negativas, versionamento | W00 |
| W02 Config e layout | `config.schema`, paths | Merge, segurança de precedência, XDG/AppData, migração | W01 |
| W03 CLI e Command API | `commands.schema` | Parse, JSON result, exit code, dispatcher único | W01/W02 |
| W04 Resolver/parser/DAG | SQL/Jinja subset, Python AST | Plan automático/manual, ciclos, dependências | W01/W02 |
| W05 Sessões/variáveis | `session.schema`, state | CRUD, revisões, snapshot, commit, TTL, IPC | W01/W03 |
| W06 Conectores SQL | Adapter contract, policies | Postgres/MySQL, execução, transação, stream | W02/W04 |
| W07 Runtime e scheduler | State machine, IPC | Execução, aninhamento, manual/auto, timeout | W03–W06 |
| W08 SDK/runtime Python | IPC, `Env` | Processos, datasets lazy, vars, `env.run`, typing | W05/W07 |
| W09 Bibliotecas e escrita | `project.schema`, connectors | Lockfile, autorização, `env.write`, Pandas/Arrow | W06/W08 |
| W10 Plugins | `plugin.schema`, SDK | Dispatcher único, overrides, stubs, plugins oficiais | W08/W09 |
| W11 IA/dicionário/lineage | Schemas e política de dados | Gateway, JSON semântico, OpenLineage, sugestões | W04/W07/W10 |
| W12 Estado robusto e auditoria | Eventos/state/recovery | AUDIT, outbox, checkpoint, chaos, replay | W05–W11 |
| W13 CI, segurança, performance | Fixtures E2E/chaos | Matriz, benchmarks, testes de regressão | Contínuo |
| W14 Linux release | Cargo/SDK, config | DEB, assinatura, upgrade/uninstall, manual | W07–W13 |
| W15 Windows release | Cargo/SDK, config | MSI, assinatura, upgrade/repair/uninstall, manual | W07–W13 |
| W16 Documentação e exemplos | APIs/verificações | Guias, exemplos executáveis, compatibilidade | Contínuo |

### 14.1 Ordem mínima de integração

1. Congelar schemas e envelope de comandos/erros antes de runtimes dependentes.
2. Validar config, política e paths antes de execução arbitrária.
3. Demonstrar `dowai plan` automático e manual em fixtures sem executar scripts.
4. Demonstrar SQL real com template condicional e variables dentro de sessão.
5. Demonstrar `env.run()` Rust↔Python, propagação de erro, run IDs e variáveis.
6. Demonstrar escrita cross-db com Pandas/Arrow sem alegação indevida de atomicidade.
7. Integrar plugins, IA, lineage, outbox e auditoria com estados reais.
8. Executar matrizes de testes e build de pacotes nativos antes do GA.

### 14.2 Primeira demonstração objetiva

```bash
dowai plan models/comissao.py --json
dowai plan --manual models/arq1.sql models/arq10.sql models/arq7.py --json
```

Critérios: primeiro comando mostra dependências estáticas sem executar Python; segundo conserva ordem dada, indica dependências ignoradas e não altera sessão ou banco. Evidência: snapshots JSON, hashes e unit/contract tests em CI. A primeira demo operacional seguinte deve executar uma sequência SQL+Python real com `var()` e `setVar()`.

### 14.3 Definição de pronto por pacote

Uma entrega só está pronta se tiver: código compilável; teste automatizado de comportamento real; validação de schemas/contratos; documentação de uso/limites; tratamento de erro e segurança; resultado de CI com SO/versões; impacto de compatibilidade/ADR quando aplicável; changelog; exemplo reproduzível. `TODO`, `pass`, mocks que contornam execução, teste não rodado e documentação isolada não representam conclusão funcional.

## 15. Plano de testes e matriz de aceite

### 15.1 Pirâmide de testes

- **Unitários Rust:** merge de config, precedência, parser, ciclos, políticas, render SQL, sessões, concorrência, estado, masking e erros.
- **Unitários Python:** SDK, tipagem, datasets, `Env`, mocks somente de unidade, subprocessos, stubs `.pyi`, exceções e variables.
- **Contratos:** JSON Schemas, golden files, comandos CLI/SDK equivalentes, IPC, adapters, OpenLineage, schemas de plugin.
- **Integração:** PostgreSQL/MySQL efêmeros, venv real, bibliotecas declaradas, plugins mockados por serviço falso, arquivos, conexões e transações.
- **E2E:** binários e SDK reais; planejar/executar modelos SQL/Python manual e automático; sem mock no caminho a ser homologado.
- **Chaos:** interrupção antes/depois de commit, perda de processo filho, timeout, falha de IPC, retry/outbox, instalação de venv interrompida.
- **Segurança:** traversal, symlink/TOCTOU, injection, segredos, TLS, pacote não autorizado, escrita externa, permissões e exfiltração por plugins.
- **Performance:** streaming maior que RAM, concorrência e throughput, memória, latência p50/p95/p99, benchmark contra baseline e hardware declarado.
- **Empacotamento:** clean install, upgrade, downgrade quando suportado, repair MSI, uninstall, PATH, SDK, config preservada, execução sem admin/root e code signing.

### 15.2 Critérios de aceite rastreáveis (preservados e atualizados)

| ID | Cenário | Assertiva obrigatória | Marco |
|---|---|---|---|
| AC001 | CLI executa `faturamento.sql` | SQL realmente enviado ao SGBD, com run_id | M2 |
| AC002 | Python usa `env.ref('faturamento')` em automático | Predecessor termina primeiro | M3 |
| AC003 | `plan comissao.py` | DAG, caminhos e ordem sem efeitos | M1/M3 |
| AC004 | Dependência A→B→A | Erro antes de qualquer efeito | M1 |
| AC005 | Referência não encontrada | Erro com localização quando possível | M1 |
| AC006 | Override de plugin local/global | Local substitui inteiro, escopo isolado | M4 |
| AC007 | Ambas APIs de plugin | Mesmo dispatcher, política e log | M4 |
| AC008 | Plugin tipado, `plugins sync` | `.pyi` reconhecido por type checker | M4 |
| AC009 | Plugin removido, repetir sync | Stub obsoleto removido, sync idempotente | M4 |
| AC010 | Referência de segredo | Nunca vaza em logs/planos/IA/stubs | M1/M5 |
| AC011 | Timeout/falha de Python | Processo encerrado, FAILED e AUDIT | M3 |
| AC012 | Fluxo incremental usuário duas vezes | Fixture não duplica chaves; no modo definido | M6 |
| AC013 | Queda antes do commit | Rollback/checkpoint anterior preservado | M6 |
| AC014 | Full refresh sem autorização | Recusa antes de escrita destrutiva | M2 |
| AC015 | REST paginado, 429 e cursor | Páginas completas, sem duplicação indevida | M4 |
| AC016 | Descontos 10%, 10,01%, 15%, 15,01% | Limiar correto de alertas | M6 |
| AC017 | Reexecução alerta | Outbox evita envio duplicado | M6 |
| AC018 | IA desativada | Núcleo funciona sem IA/credencial | M3/M5 |
| AC019 | SQL inválido com IA | Sugestão, nenhuma aplicação automática | M5 |
| AC020 | Dicionário global/local mesmo ID | Local prevalece; origem auditável | M5 |
| AC021 | Grain/referência inválida | Validação falha sem inventar fórmula | M5 |
| AC022 | dbt subset suportado | Resultado comparável por fixture | M2 |
| AC023 | Macro dbt não suportada | `UNSUPPORTED` explícito | M2 |
| AC024 | Invocação por Airflow/Dagster | JSON/exit codes versionados | M2 |
| AC025 | Geração de lineage | OpenLineage schema válido, inferência separada | M5 |
| AC026 | Dataset maior que RAM | Streaming dentro do limite configurado | M6 |
| AC027 | Fluxo AUDIT | IDs/hashes/contagens reais, sem segredo | M5 |
| AC028 | `init` Linux sem root | Workspace criado com permissão adequada | M1 |
| AC029 | `doctor` | Detecta erro de config/SDK/plugin | M4 |
| AC030 | Path traversal/symlink | Operação negada com erro de política | M1/M5 |
| AC031 | JSON de config nativo | Schema, defaults, merge e origem corretos | M1 |
| AC032 | JSON + YAML legado no mesmo escopo | Conflito diagnosticado sem merge ambíguo | M1 |
| AC033 | Diretórios XDG/AppData | Paths e override configurados corretamente | M1/M7 |
| AC034 | Mesma operação via CLI/Python | Plano, política, status e IDs equivalentes | M3 |
| AC035 | Sessão CLI explícita entre processos | `--session` recupera variables no mesmo usuário | M3 |
| AC036 | Variável de CLI em template SQL | Ramificação correta, sem injection | M2 |
| AC037 | `def_var` SQL seguido de Python | Valor disponível na mesma sessão | M3 |
| AC038 | `setVar` Python seguido de SQL | Valor disponível em `var()` e condicional | M3 |
| AC039 | Falha ao atualizar variable | Mudança não confirmada é descartada | M3 |
| AC040 | Escritas concorrentes na mesma variável | Conflito explícito, sem last-writer-wins | M3 |
| AC041 | `run --manual a b c` | Ordem de argumentos sem auto-predecessores | M3 |
| AC042 | `with env.manual()` | Ordem preservada, modo restaurado na saída | M3 |
| AC043 | `env.run` dentro de Python | Sem aresta DAG; evento filho no histórico | M3 |
| AC044 | Modelo chamado com deps em automático/manual | Automático executa deps; manual não executa | M3 |
| AC045 | Falha sem `try` | Sequência interrompida, próximas não iniciam | M3 |
| AC046 | Falha capturada em `try` | Chamador segue, filho permanece FAILED auditado | M3 |
| AC047 | Chamada manual repetida | Duas invocações reais e auditáveis | M3 |
| AC048 | Recursão imperativa entre arquivos | Limite/ciclo detectado e finalizado | M3 |
| AC049 | Pandas/Arrow cross-db autorizado | Schema/linhas gravadas e auditoria coerente | M4 |
| AC050 | Biblioteca não declarada/não autorizada | Ambiente/requisição negado com diagnóstico | M4 |
| AC051 | Limites `max_threads`/mem/processos | Enforcement ou limitação documentada/testada | M3/M6 |
| AC052 | Projeto tenta ampliar limite administrativo | Config efetiva mantém teto; violações falham | M1 |
| AC053 | Condicional `{{ if ... }}` e `{% if ... %}` | AST/output equivalentes com testes negativos | M2 |
| AC054 | Linux DEB clean/upgrade/uninstall | Executável, config e dados preservados | M7 |
| AC055 | Windows MSI clean/repair/upgrade/uninstall | Executável, PATH opcional, dados preservados | M7 |
| AC056 | Cancelamento manual/automático | Processos limpos, estado CANCELLED, code 130 | M3/M6 |
| AC057 | `plan` não altera estado | Sem efeitos, mesmo com `def_var`/macros | M1/M2 |
| AC058 | Escrita direta via biblioteca | Permissão aplicada e observabilidade parcial declarada | M4 |
| AC059 | Templates com refs condicionais | União conservadora estática e snapshot de execução | M2 |
| AC060 | Sessão expirada/inválida | Recusa sem vazamento de estado entre projetos/usuários | M3 |

Cada AC exige fixture, automação, saída esperada, ambiente, commit, link de evidência e estado real. ACs implementados parcialmente não passam como `VERIFIED`. Adicionar testes de regressão para cada correção pública.

### 15.3 Matriz de plataformas e dependências

Antes de cada release, publicar e testar a matriz efetiva:

| Eixo | Requisito |
|---|---|
| Linux | Distribuições e arquiteturas oficialmente suportadas, versão mínima de glibc/alternativa |
| Windows | Edições e arquiteturas suportadas, runtime C/C++ quando necessário, permissões |
| Python | Versões de CPython homologadas por SO e estratégia de distribuição/descoberta |
| Rust | Toolchain fixada em CI e lockfile versionado |
| Bancos | Versões de PostgreSQL/MySQL, TLS, capabilities/driver por SO |
| Bibliotecas | Pandas/Polars/PyArrow/SQLAlchemy e resolvedor de lockfile testados |
| dbt | Versão(s) de fixtures de compatibilidade, subset exato |
| IA/OpenLineage | Schema/API e provider versions validados e documentados |

Nenhuma versão específica é considerada suportada por mera inferência. Usar imagens/VMs limpas e artefatos binários de release reais nos smoke tests.

## 16. Empacotamento e distribuição

### 16.1 Linux

- Produzir `.deb` instalável, versionado e assinado conforme processo da distribuição; disponibilizar tarball quando útil.
- Binário Rust e arquivos somente de leitura em locais apropriados; nenhuma criação de workspace de usuário durante instalação do pacote como root.
- `dowai init`, `doctor`, `plan` e exemplo real devem funcionar sem sudo.
- APT repository e comando `apt install dowai` só devem ser anunciados **depois** de repositório publicado, indexado e assinatura verificada. `.deb` isolado não equivale a repo APT.
- Testar install/upgrade/reinstall/remove/purge e comportamento de arquivos de configuração/persistência; segredos e projetos nunca destruídos silenciosamente.

### 16.2 Windows MSI

- Produzir MSI por arquitetura suportada, assinado nas releases públicas e construído por ferramenta de empacotamento versionada (por exemplo, WiX, sujeito a avaliação técnica/compatibilidade).
- Instalação por máquina em `%ProgramFiles%\DOWAI\` quando apropriado; oferecer modalidade por usuário somente após teste real de permissões e upgrade.
- Opção de adicionar `dowai.exe` ao `PATH`; não modificar PATH global sem opção/política clara.
- Instalar binário, documentação/metadata necessárias e suporte a integração Python; estratégia de Python embutido vs externo deve ser fechada por ADR com teste de licença, tamanho e atualização.
- Suportar silent install, update, repair e uninstall; não apagar `Documents\DOWAI`, `%APPDATA%\DOWAI` ou `%LOCALAPPDATA%\DOWAI` com dados pessoais por default.
- Testar contas sem privilégio, espaços e Unicode no path, nomes longos, Windows Defender/SmartScreen no cenário suportado, upgrade e desinstalação.
- Produzir hashes/checksums, SBOM, informações de versão, release notes, assinaturas e instruções reproduzíveis.

### 16.3 Gates comuns de release

1. CI verde de unidade, contratos, integração e E2E obrigatórios em plataformas suportadas.
2. Matriz de compatibilidade publicada e fixture/db de teste comprovada.
3. Nenhum segredo de teste incorporado a binário, logs ou pacote.
4. Hashes, origem de builds, lockfiles, SBOM, licença e política de disclosure verificados.
5. Pacotes instalados **em máquina limpa** com scripts de smoke test, upgrade e desinstalação.
6. `dowai --version`, `doctor`, `init`, `validate`, `plan`, `run` SQL/Python e modos manual/automático executados nos pacotes distribuídos.
7. Manual de instalação, configuração XDG/AppData, troubleshooting, backup/recovery e limitações publicado.
8. Artefatos públicos no destino oficial, checksums verificados e links testados antes de anunciar GA.

## 17. Casos de uso normativos completos

### 17.1 SQL declarativo

`sources.json` descreve `postgres_vendas.clientes`/`vendas`; `stg_clientes.sql` lê fonte; `faturamento.sql` referencia `stg_clientes`. `dowai run faturamento.sql` faz plano → predecessor → faturamento, tudo no SGBD, registra run_id/linhagem. Segunda execução obedece regras da materialização; não depende da ordenação alfabética.

### 17.2 Condicional SQL com variável de sessão

```bash
dowai session start --json
dowai run carga.py --session <session_id> --vars '{"carga_total":true}'
```

Em `carga.py`, `env.run("faturamento.sql")` herda a sessão; o SQL utiliza `{% if var('carga_total') == true %}` para escolher consulta full ou com filtro. `def_var('carga_total', false)` não sobrescreve valor CLI. Um segundo comando com `--session` vê alterações confirmadas por Python/SQL. Novo run sem `--session` não herda variáveis.

### 17.3 Manual com tratamento de erro

```python
from dowai import DowaiExecutionError

def model(env):
    env.setVar("carga_total", True)
    with env.manual():
        env.run("arq1.sql")
        try:
            env.run("arq10.sql")
        except DowaiExecutionError:
            env.log.warn("Falha tolerada por decisão do modelo")
        env.run("arq7.py")
```

Sem `try`, `arq7.py` não é iniciado quando `arq10.sql` falha. Não há `cancelHierarchy()`. Nenhum `env.run` insere aresta estática no DAG; a árvore de invocações preserva ordem e pai-filho. As dependências próprias dos modelos só são executadas automaticamente fora do contexto manual.

### 17.4 Python, Pandas e outro banco

`comissao.py` usa `env.ref("faturamento")` → `to_pandas()` (com aviso/limite de memória) → cálculo → `env.write(df, target="postgres_financeiro", table="comissoes", mode="append")`. Perfil autorizado, schema validado, escrita transacional no destino e auditoria. Para datasets grandes, substituir por batches Arrow. Falha cross-db não implica rollback da origem.

### 17.5 Alertas com plugin

`faturamento_comissao.py` usa desconto para enviar email ao supervisor acima de 10% e ao gerente acima de 15%. Plugin local `email` substitui global. `idempotency_key` e outbox previnem duplicidade em retries. Limiares exatos são testados em 10%, 10,01%, 15% e 15,01%.

### 17.6 REST e tratamento de falhas

Plugin REST autorizado lê três páginas por cursor, recebe 429 transitório, respeita backoff configurado e registra contagens sem duplicação. Falha permanente retorna FAILED; política de retries não reexecuta efeito não idempotente sem chave.

### 17.7 dbt e IA

Projeto dbt pequeno com `ref/source/config` gera relatório de compatibilidade e resultados comparáveis quando suportados. Dicionário local substitui métrica global para IA, com origem visível; `env.ia` pode gerar sugestão/documentação, mas não executar alteração automaticamente.

## 18. Decisões técnicas ainda sujeitas a ADR

**Os requisitos funcionais acima estão aprovados.** Os itens abaixo são escolhas de implementação que precisam ser congeladas **antes** de módulos dependentes, com alternativas, riscos, testes e decisão registrada; não justificam rediscutir produto já aprovado.

| ID | Questão a fechar | Default técnico provisório | Ponto de congelamento |
|---|---|---|---|
| D01 | Versões SO/Python/Rust/Postgres/MySQL | Menor matriz testável que permita GA Win/Linux | M0/M7 |
| D02 | Backend de estado/sessão e migração | SQLite local transacional | Antes de W05 |
| D03 | IPC Rust↔Python (transport/auth) | Mensagens JSON + Arrow para dados | Antes de W08 |
| D04 | Gerenciador Python/lockfile | Um resolvedor estável, reproduzível | Antes de W09 |
| D05 | Interface de conexão nativa para Pandas | Capability objeto, não URL com segredo exposto | Antes de W09 |
| D06 | Enforce de CPU/memória no Windows/Linux | Melhor esforço com recursos de SO, limites reportados | Antes de M3/M7 |
| D07 | Empacotamento SDK/Python no MSI | Binário Rust + descoberta/instalação guiada de Python | Antes de W15 |
| D08 | Políticas de retry de side effects e outbox | Idempotency key obrigatória quando necessário | Antes de M4/M6 |
| D09 | Estratégia de incremental/watermark de exemplos | Regras de usuário e checkpoint por commit | Antes de M6 |
| D10 | Proveniência de IA/retention/LGPD | Dados não enviados por default | Antes de M5 |
| D11 | Versionamento OpenLineage/facets | Schema oficial fixado em fixtures | Antes de M5 |
| D12 | Compatibilidade dbt por versão | Subconjunto mínimo testado | Antes de M2 |
| D13 | Política de assinatura/publicação DEB/MSI | Assinatura e checksums em GA | Antes de M7 |
| D14 | Persistência de sessões CLI e locking | Reabrir somente por ID/usuário com TTL | Antes de W05 |
| D15 | Transações de variáveis em chamadas aninhadas | Commit por invocação bem-sucedida | Antes de W05/W07 |
| D16 | Compatibilidade de aliases de templates `{{ if }}` | Parser normaliza aliases DOWAI explicitamente | Antes de W04 |
| D17 | Restrição de leitura/escrita direta por bibliotecas | Política declarativa + controles OS onde viáveis | Antes de W09 |

Decisões que alterarem comportamento, segurança, escopo ou custo exigem validação de produto. Diferenças reversíveis de organização interna podem ser resolvidas por ADR técnico. Nunca preencher uma decisão pendente com suposição silenciosa em código.

## 19. Riscos e mitigação

| Risco | Mitigação/controle |
|---|---|
| Ordem manual gera dados inconsistentes | Aviso no plano, auditoria, política autorizadora, testes sem auto-deps |
| Chamadas imperativas recursivas | Pilha de invocações, limite de profundidade e timeout |
| Variáveis concorrentes não determinísticas | Snapshot/revisões, conflito e commit explícito |
| Template SQL injeta valores perigosos | Bind parameters, allowlist AST e testes de injection |
| Contexto vaza entre sessões/projetos | IDs autenticados, scope, TTL e testes de isolamento |
| Python/Pandas consome toda RAM | Streaming, tamanho de batch, limites e observabilidade |
| Biblioteca autorizada age fora de política | Menor privilégio, sandbox quando suportado, sem promessa de isolamento por venv |
| Cross-db apresenta sucesso parcial | Transações separadas, staging, checkpoint e recovery |
| Efeitos externos duplicam em retry | Outbox, idempotency key, replay e chaos tests |
| Compatibilidade dbt fictícia | Fixtures reais, relatório granular e matriz por versão |
| OpenLineage não conforme | Validação de schema oficial, facets versionadas |
| IA divulga segredos/inventa regra | Opt-in, redaction, revisão humana, sem autoexecução |
| Divergência CLI/SDK | Application Command API única, suite de paridade |
| MSI/DEB não instala em ambiente limpo | CI multiplataforma, clean VM, assinatura e smoke test |
| Parâmetros mudam sem rastreabilidade | Schema, defaults documentados, effective config e hash por run |

## 20. Documentação operacional, rastreabilidade e encerramento

### 20.1 Artefatos obrigatórios em manutenção contínua

| Artefato | Conteúdo e cadência |
|---|---|
| `docs/STATUS.md` | Estado real por pacote/AC, evidência, bloqueios a cada integração |
| `docs/ROADMAP.md` | Dependências, marcos, decisões, mudanças de sequência |
| `docs/ADR/*.md` | Contexto, alternativas, decisão, impacto, testes antes de mudança de contrato |
| `specs/*.schema.json` | APIs/config/eventos/IPC versionados e fixtures |
| `docs/API.md` | SDK, CLI, retorno/erros, manual/auto, variáveis, datasets |
| `docs/CONFIGURATION.md` | Todos os parâmetros, defaults, limites, precedência e exemplos |
| `docs/COMPATIBILITY.md` | SO, versões, drivers, dbt, bibliotecas e gaps testados |
| `docs/OPERATIONS.md` | Instalação, sessão, backup, recuperação, logs, upgrades |
| `docs/SECURITY.md` | Threat model, sandbox, permissões, secrets, disclosure |
| `docs/PERFORMANCE.md` | Workloads, hardware, resultados, baseline, limitações |
| `CHANGELOG.md` | SemVer, migrações, breaking changes e release notes |
| Matriz AC→teste→commit→CI→artefato | Evidência de aceite de cada requisito, por release |

### 20.2 Checklist de início de desenvolvimento

1. Inventariar repositório/código existente e marcar implementação real, sem pressupor que scaffold anterior compila.
2. Fixar contratos e ADRs bloqueantes de M0/M1; definir convenções de erro, schema e logging.
3. Criar `config.schema.json`, `commands.schema.json`, `session.schema.json` e fixtures.
4. Configurar CI, lint, testes de contrato e matriz inicial Windows/Linux.
5. Implementar CLI `init/validate/plan` com modo manual e automático sem efeitos.
6. Provar resolução XDG/AppData, precedência, tetos de segurança e `config show --effective`.
7. Estabelecer evidência de cada teste e relatório de status antes de avançar ao runtime real.

### 20.3 Critério final de produto entregue

O DOWAI estará pronto para primeira distribuição pública quando Linux e Windows possuírem artefatos oficiais instaláveis, a matriz de suporte estiver publicada, todos os ACs do escopo da release estiverem `VERIFIED`, os gates de segurança/robustez/performance estiverem aprovados e os exemplos SQL, Python, manual, automático, variáveis de sessão, plugins, escrita cross-db, auditoria e operação sem IA executarem **no produto empacotado**. Funcionalidades fora do escopo devem ser claramente marcadas, sem simulacros nem promessas de compatibilidade não testada.

---

## Apêndice A — Referências técnicas oficiais para verificação na implementação

Estas referências orientam decisões técnicas; suas versões precisam ser fixadas na documentação de compatibilidade e revisitadas em cada release relevante.

- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir/0.8/) — localização de config/estado/cache no Linux.
- [Microsoft Windows Installer: installation context](https://learn.microsoft.com/en-us/windows/win32/msi/installation-context) e [Windows Installer best practices](https://learn.microsoft.com/en-us/windows/win32/msi/windows-installer-best-practices) — instalação por usuário/máquina e MSI.
- [OpenLineage facets and extensibility](https://openlineage.io/docs/spec/facets/) — estrutura de eventos e facets de lineage.
- [Python Packaging: dependency specifiers](https://packaging.python.org/en/latest/specifications/dependency-specifiers/) e [dependency groups](https://packaging.python.org/en/latest/specifications/dependency-groups/) — declarações e ferramentas de dependência Python.
- Documentação oficial de Rust, Cargo, Tokio, Rayon, CPython, dbt, PostgreSQL, MySQL, Arrow IPC e gerenciador de pacotes escolhido deverá ser referenciada por versão nas ADRs e matriz, sem pressupor compatibilidade não testada.

## Apêndice B — Glossário curto

- **DAG**: grafo acíclico de dependências estáticas; **não** se confunde com histórico de chamadas `env.run()`.
- **Modo automático**: planeja e executa predecessores; **modo manual**: executa ordem fornecida, sem predecesso automático.
- **Sessão**: contexto de variáveis e invocações relacionadas; **run**: execução específica; **invocation**: chamada de um modelo; **attempt**: tentativa/retry.
- **Template SQL**: renderização controlada de condicionais e macros antes de enviar SQL ao banco; **bind parameter**: valor SQL passado ao driver sem concatenação textual insegura.
- **Adapter**: driver/camada de capabilities para um SGBD; **lazy dataset**: relação que posterga transferência/materialização.
- **IPC**: protocolo interprocesso Rust↔Python; **Arrow**: formato de dados colunar adequado a batches.
- **Outbox/idempotência**: mecanismos de recuperação/deduplicação de efeitos externos; **checkpoint/watermark**: progresso durável confirmado pelo fluxo.
- **AUDIT**: trilha de evento independente da severidade de logs; **OpenLineage**: contrato padronizado de lineage.
- **ADR**: registro de decisão arquitetural; **Definition of Done**: condições verificáveis para declarar uma entrega concluída.

A API `env.cancelHierarchy()` está formalmente excluída. Alterações posteriores exigem documento versionado/ADR e migração compatível.

