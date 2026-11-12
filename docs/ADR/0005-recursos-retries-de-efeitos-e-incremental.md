# ADR 0005: Recursos, retries de efeitos externos e incremental

- Status: **Accepted** — o GP adotou os defaults técnicos provisórios do plano (§18, D06, D08 e D09) como decisão vigente. Não altera requisito de produto aprovado. Reabrir qualquer item exige nova decisão registrada em `_gestao/DECISOES.md`.
- Date: 2026-11-12
- Decide: D06 (enforce de CPU/memória por SO), D08 (retry de efeitos externos e outbox com idempotency key) e D09 (watermark/incremental dos exemplos).
- Referências: `plano_mestre.md` §0.3 (P20, P22), §3.4 (catálogo, grupos `execution`, `python`, `plugins`), §5.4, §9.3, §10.2, §10.3, §12.1, §15.2 (AC051), §17.5 e §17.6; §18 (D06, D08, D09).
- Tarefa: ENT-006.

## Contexto

O plano exige que limites operacionais sejam documentados e reportados (P20, AC051), que efeitos externos não sejam duplicados em retries (§9.3, §17.6) e que o incremental seja definido pelo usuário, não inferido pelo motor (P22, §10.3). Os três pontos estavam abertos em §18 com defaults provisórios. Antes de M3, M4 e M6, precisam de regra verificável.

Estado do código verificado em 2026-11-12: não há enforcement de recursos, retry de efeito externo, outbox nem checkpoint implementados. As regras abaixo são o contrato para ENT-037, ENT-038, ENT-051, ENT-062 e ENT-066.

## Decisão

### D06 — Enforcement de CPU e memória

Princípio: **melhor esforço com recursos do SO, e o nível realmente aplicado é sempre reportado**. Nenhum limite pode ser apresentado como garantido sem teste na plataforma correspondente.

Cada limite reportado recebe um dos estados abaixo, persistido no run e exibido em `dowai run`, nos logs e no AUDIT:

- `enforced`: o SO aplicou o limite e o runtime verificou a aplicação.
- `partial`: aplicado com ressalva documentada (por exemplo, memória virtual em vez de residente).
- `not_enforced`: o SO ou a configuração não permite o limite; o valor configurado é apenas informativo.

Mecanismos por plataforma:

| Recurso | Linux | Windows |
|---|---|---|
| Memória do processo filho (Python/plugin) | `RLIMIT_AS` aplicado no subprocesso antes da execução. Limita memória **virtual**, que é mais conservadora que a residente; o estado é `partial`. | Job Object com `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (limite por processo) ou `JOB_OBJECT_LIMIT_JOB_MEMORY` (limite do grupo). Estado `enforced`. |
| CPU | `RLIMIT_CPU` limita tempo total de CPU, não percentual; o percentual configurado fica `not_enforced`. | Job Object com `JOBOBJECT_CPU_RATE_CONTROL` (percentual). Estado `enforced`. |
| Número de processos filhos | `RLIMIT_NPROC` conta **todos** os processos do usuário e não é aplicado por padrão (`not_enforced`), para não afetar outros processos da máquina. | `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` no Job Object do run. Estado `enforced`. |
| Timeout de nó e de run | Responsabilidade do motor (ENT-039), não do SO. | Idem; ver ENT-039 para cancelamento em Windows. |

Regras:

1. Cgroups do Linux não são o mecanismo padrão, porque exigem privilégio ou delegação do systemd. Podem ser avaliados em decisão futura, sem alterar o default.
2. O parâmetro `python.resource_enforcement` aceita `best_effort` (default) e `require`. Com `require`, um limite que não possa ser `enforced` ou `partial` falha o nó com código `DOWAI_RESOURCE_UNSUPPORTED`, antes de executar o código do usuário. Esse parâmetro deve entrar no catálogo de ENT-012 com tipo, default e validação.
3. Mesmo com `best_effort`, o estado de cada limite é gravado e exibido. Não há limite silenciosamente ignorado.
4. Os limites são somados por run apenas até onde a plataforma permite; a documentação (ENT-079, `docs/OPERATIONS.md`) deve declarar essa limitação.
5. Os testes de enforcement de Linux devem usar margem, porque `RLIMIT_AS` interage com bibliotecas que reservam memória virtual.

Validação prevista: teste de Linux com subprocesso que excede `RLIMIT_AS` e recebe erro de memória (ENT-038). Teste de Windows com Job Object depende de máquina Windows (ENT-093) e **não foi executado nesta decisão**.

### D08 — Retry de efeitos externos e outbox

**Operações com efeito externo** (exigem idempotency key quando houver retry ou outbox):

1. `env.write` para destino em banco diferente da origem, nos modos `append`, `replace` e `upsert` (ENT-048).
2. Gravação de arquivos em destino remoto ou compartilhado: `env.generateCsv` para SFTP ou caminho externo, e o plugin `files` (ENT-050, ENT-057).
3. Plugin `rest` com método que altera estado (`POST`, `PUT`, `PATCH`, `DELETE`) (ENT-058).
4. Plugin `email`, em cada envio (ENT-059).
5. Plugin `db`, na escrita do destino (ENT-060).
6. Plugin `alerts`, em cada notificação ou ação disparada (ENT-061).
7. Qualquer SQL escrito pelo usuário que altere dados e não seja materialização `table`/`view` gerenciada pelo motor. Esse SQL nunca tem retry automático.

**Operações sem efeito externo ou repetíveis por design** (retry permitido pela política):

- Leituras: `GET`/`HEAD` REST, `SELECT`, leitura de arquivo, `env.ref`, `env.source`, `env.connections.get`.
- Materialização `table`/`view` gerenciada pelo motor, feita por staging e swap (ENT-033), que substitui o alvo de forma atômica.
- Chamadas de IA (ENT-068): não alteram dados externos. Retry só com política explícita, pelo custo e pela variabilidade da resposta.

**Regras de retry:**

R1. `retry_max_attempts` conta o total de tentativas por nó, incluindo a primeira. O default `1` significa **nenhum retry automático**, conforme o exemplo de §3.5.

R2. Retry automático só ocorre quando **ambas** as condições são verdadeiras:
- o erro é classificado como transitório: timeout de conexão ou de consulta, deadlock ou falha de serialização, HTTP `429`, `503` ou `504`, ou queda de conexão antes da confirmação do envio;
- a operação é idempotente comprovada (lista de leitura acima ou materialização gerenciada) **ou** uma idempotency key é enviada e aceita pelo destino ou pelo plugin.

R3. Erros permanentes não são repetidos: validação, schema mismatch, autenticação `401`/`403`, SQL inválido, violação de constraint. Falham o nó com `FAILED`.

R4. Cada retry cria novo `attempt_id` (ENT-037). A invocação e o run mantêm o mesmo identificador.

R5. O intervalo usa backoff exponencial com jitter, com base em `retry_backoff_ms` e teto configurável (ENT-037).

R6. **Idempotency key** é determinística e derivada de `run_id`, `node_id`, índice da invocação e nome da operação. Ela não muda entre tentativas da mesma invocação. É gravada na outbox antes do envio e enviada ao destino quando o protocolo suportar (por exemplo, header `Idempotency-Key` em REST).

R7. **Outbox durável:** o efeito externo é registrado como `PENDING` antes do envio, passa a `SENT` após o envio e a `CONFIRMED` após a confirmação do destino. Falhas em `FAILED` são permanentes. Quando o estado não puder ser determinado após uma falha, o estado é `UNCERTAIN`. Replay usa a mesma chave. A outbox não cria transação distribuída: falha entre commit do banco e entrega é recuperável, mas não atômica (§9.3).

R8. Quando o destino não aceita chave e não há verificação prévia possível, a operação **não** é repetida automaticamente. O estado vai para `UNCERTAIN` e a ação passa ao usuário, por `dowai retry` (ENT-036, ENT-062).

R9. `plugins.require_idempotency_key` default `true`: plugin que execute efeito externo sem chave é recusado na validação de manifesto (ENT-052).

### D09 — Incremental e watermark dos exemplos

1. O motor **não infere** regra incremental. O usuário declara filtro, chave, coluna de watermark, exclusões, modo UPSERT e condição de confirmação (§10.3, P22).
2. O watermark é um valor tipado e comparado explicitamente (timestamp, inteiro ou texto ordenável). Ausência de checkpoint significa que a primeira execução é carga completa, desde que o usuário a declare.
3. O checkpoint persiste o watermark **somente após commit confirmado** da escrita. Falha antes do commit preserva o checkpoint anterior (§10.3, ENT-062).
4. Nos exemplos (ENT-086 e demais exemplos incrementais), a coluna de watermark é a declarada no próprio exemplo, sem heurística.
5. O teste obrigatório do fluxo de exemplo é: segunda execução sem dados novos não insere nem duplica linhas, e uma execução com dados novos carrega apenas o delta. A chave de UPSERT impede duplicidade mesmo se o checkpoint for perdido após o commit.
6. A garantia é **at-least-once com UPSERT por chave** para o fluxo de exemplo. Não é garantida para SQL arbitrário do usuário, conforme §10.3.
7. CDC e tombstone ficam fora do MVP.
8. `materialized='incremental'` do dbt continua `UNSUPPORTED` (ADR 0004). A ativação depende de ENT-032 e ENT-033, com teste transacional.

## Alternativas consideradas

- **Cgroups como mecanismo padrão no Linux (D06):** enforcement mais preciso de CPU e memória residente, mas exige privilégios ou delegação do systemd, o que contraria o princípio de não configurar serviços persistentes. Rejeitado como default; avaliável no futuro.
- **Exigir `require` como default (D06):** garantiria limites, mas quebraria execuções em Linux sem `RLIMIT` efetivo e em Windows sem validação real. Rejeitado como default.
- **Retry automático por padrão para erros transitórios (D08):** melhora a robustez, mas pode duplicar efeitos externos sem chave. Rejeitado pela política de idempotência do plano.
- **Transação distribuída entre origem e destino (D08):** o plano não promete atomicidade entre bancos (§10.2) e o motor não controla o destino. Rejeitado.
- **Inferir watermark automaticamente (D09):** viola P22 e pode perder dados silenciosamente. Rejeitado.
- **CDC ou tombstones no MVP (D09):** exigem suporte de cada banco e não estão aprovados. Adiados.

## Consequências

- ENT-012 deve incluir `python.resource_enforcement` no catálogo, com default `best_effort`.
- ENT-036 deve incluir o estado `UNCERTAIN` na lista de estados persistidos.
- ENT-037 implementa R1 a R5; ENT-051 implementa R6 a R9; ENT-062 implementa o checkpoint de D09; ENT-066 aplica D09 ao incremental dirigido pelo usuário.
- Documentação (ENT-079 e ENT-080) deve declarar a limitação de enforcement e a garantia at-least-once do exemplo, sem prometer atomicidade.
- Nenhum comportamento foi implementado nesta decisão. Este ADR define o contrato das tarefas dependentes.

## Testes associados

Nenhum teste de código foi criado ou executado por esta decisão. O critério de aceite de ENT-006 é documental: o ADR cobre Windows e Linux (D06) e lista as operações com efeito externo que exigem idempotency key (D08). A verificação executada nesta tarefa está registrada em `_gestao/EVIDENCIAS.md`.

Testes que comprovam este ADR pertencem às tarefas dependentes citadas em Consequências, principalmente ENT-038 (enforcement Linux), ENT-039 e ENT-093 (Windows), ENT-037 (retry), ENT-051 (outbox), ENT-062 e ENT-066 (checkpoint e incremental).
