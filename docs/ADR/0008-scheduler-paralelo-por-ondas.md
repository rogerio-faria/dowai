# ADR 0008: Scheduler paralelo por ondas, publicação de variáveis e detecção de conflitos

- Status: **Accepted** — escolhas conservadoras derivadas da opção 1 aprovada para ENT-105 (`_gestao/DECISOES.md`, D-092 e D-128).
- Date: 2026-11-13
- Decide: a forma do paralelismo automático entre nós (`plano_mestre.md` §5.5 e §4.4 "conflitos devem ser detectados e reportados").
- Referências: `plano_mestre.md` §4.4, §5.5; ADR 0002 (variáveis de sessão e transações); ADR 0005 (recursos e retries); ADR 0006 (versões); `docs/CONFIGURATION.md` (`runtime.max_parallel_nodes`, `runtime.max_worker_processes`); D-060 (`RLIMIT_AS` por worker).
- Tarefa: ENT-119 (derivada de ENT-105; implementação em ENT-120; base de código em ENT-118).

## Contexto

Hoje o agendador executa os nós em sequência: `runtime.max_parallel_nodes` é validado e exibido, mas a concorrência efetiva é 1 (`docs/CONFIGURATION.md`; aviso em `resources.rs`). O plano prevê concorrência automática limitada por recursos, mantendo o modo manual sequencial por contrato. Paralelizar nós que leem e gravam variáveis de sessão exige regras explícitas para evitar `last-writer-wins` silencioso.

## Decisão

### 1. Ondas

- As ondas são os níveis topológicos do DAG do plano.
- Um nó só entra em uma onda quando todas as suas dependências já foram **publicadas** (concluídas com sucesso e com variáveis publicadas).

### 2. Limite de concorrência

- Simultaneidade máxima da onda: `min(runtime.max_parallel_nodes, runtime.max_worker_processes)`.
- O modo **manual** (`dowai run --manual`, `env.manual()`) permanece sequencial por contrato, sem paralelismo.
- O modo automático sem nós independentes equivale a sequencial.

### 3. Snapshot e publicação de variáveis

- Cada nó de uma onda recebe o snapshot de variáveis do **início da onda**.
- As variáveis gravadas pelos nós da onda são publicadas **na ordem do plano**, ao fim da onda, somente se a onda não tiver conflito e todos os nós tiverem sucesso.

### 4. Conflito

- Dois ou mais nós da mesma onda que gravem a mesma variável geram `DOWAI_VARIABLE_CONFLICT`.
- Nenhuma variável da onda é publicada; o run termina `FAILED`.
- Não há resolução automática (sem last-writer-wins).

### 5. Conexões

- Cada nó paralelo usa **uma conexão própria** (ex.: PostgreSQL), sem compartilhar a conexão do run.
- `env.write` e sessões de escrita permanecem por nó.
- Conexões compartilhadas entre nós ficam fora de escopo.

### 6. Journal e AUDIT

- Journal e eventos AUDIT são gravados em ordem determinística pelo índice do nó no plano, independentemente da ordem de conclusão real.
- Execuções repetidas com as mesmas entradas produzem a mesma sequência de eventos lógicos.

### 7. Falha

- Falha de um nó cancela os nós pendentes da mesma onda e aplica a política de falha já existente (`execution.stop_on_error`).
- Efeitos já enviados à outbox (ENT-051) permanecem na outbox; não são revertidos pelo agendador.
- Nós de ondas posteriores ficam `SKIPPED`/`CANCELLED` conforme a política.

### 8. Recursos

- `RLIMIT_AS` continua aplicado por worker (D-060).
- O cálculo de memória do run considera a concorrência máxima da onda, não o total de nós.

## Alternativas consideradas

1. **Manter sequencial para sempre** — descartada: não atende a concorrência automática prevista no plano.
2. **Snapshots completos por nó com merge automático** — descartada nesta fase: aumenta a superfície de semântica de variáveis e exige nova decisão de produto (alternativa 3 de D-092, não aprovada).
3. **Last-writer-wins com aviso** — descartada: viola o plano ("não usar last-writer-wins silencioso").

## Consequências

- ENT-120 deve implementar exatamente estas regras.
- `runtime.max_parallel_nodes` só passa a ter concorrência efetiva 1 quando não houver nós independentes ou o paralelismo estiver desativado; o aviso em `resources.rs` deve refletir isso.
- `docs/CONFIGURATION.md` e `docs/OPERATIONS.md` devem ser atualizados em ENT-120.

## Testes exigidos por ENT-120

1. Sobreposição real: 4 nós independentes com limite 4 têm intervalos de execução sobrepostos; com limite 2, o máximo simultâneo medido é 2.
2. Modo manual: execução sequencial, sem sobreposição (regressão).
3. Conflito: dois nós da mesma onda gravando a mesma variável resultam em `DOWAI_VARIABLE_CONFLICT`, run `FAILED`, sem publicação parcial.
4. Determinismo: journal e AUDIT na ordem do plano, em execuções repetidas.
5. Falha: falha de um nó interrompe os pendentes da onda; efeitos já enviados ficam na outbox.
6. Avisos: aviso de concorrência efetiva 1 só quando o paralelismo não estiver ativo; `resource_limits_e2e` atualizado.
