# Desempenho e benchmarks

Este documento registra workloads, hardware, resultados e limitações do comando `dowai benchmark`
(ENT-076). Os números abaixo são medições reais, não estimativas.

## Comando

```
dowai benchmark <file_or_model>... [--repeat N] [--json]
```

- `N` vai de 1 a 20 (padrão 3).
- Cada repetição é um run comum; o relatório traz duração por run, mediana, pico de memória dos
  processos Python (worker), o teto `runtime.max_memory_mb` e o host (SO, kernel, CPU, núcleos,
  RAM total).
- `--json` produz o relatório estruturado (`schema_version` 1).

## Medições de memória

- Pico de memória: maior RSS de processo filho (worker Python) observado pelo processo `dowai`,
  lido com `getrusage(RUSAGE_CHILDREN)`. É um valor acumulado sobre o benchmark, não por repetição.
  Só é medido em Linux; em outras plataformas aparece como "não medido".
- Teto: `runtime.max_memory_mb` é aplicado como `RLIMIT_AS` por worker (ENT-038). É limite de
  memória virtual, não de RAM física.

## Workload medido: `examples/benchmark-stream`

Um modelo Python gera 6 000 000 de linhas com `env.generateCsv` (streaming), relê o CSV linha a
linha e publica contagens. O CSV tem 171 177 797 bytes.

Configuração: `runtime.max_memory_mb = 128`, `python.interpreter` apontando para o venv do projeto.

Comando: `dowai benchmark models/stream_count.py --repeat 3 --json`

| Repetição | Duração (ms) | Status    |
|-----------|-------------:|-----------|
| 1         | 51 236       | SUCCEEDED |
| 2         | 52 390       | SUCCEEDED |
| 3         | 49 292       | SUCCEEDED |

- Mediana: 51 236 ms; mínimo 49 292 ms; máximo 52 390 ms.
- Pico de RSS do worker: 20 804 KiB (~20 MiB), dentro do teto de 128 MiB (`within_memory_limit: true`).
- Dataset (171 MB) maior que o teto de memória (128 MiB), processado sem materializá-lo em memória.

Hardware de referência (declarado pelo host no relatório):
- Linux 6.8.0-149-generic, x86_64.
- CPU: AMD EPYC Processor (with IBPB), 4 núcleos lógicos.
- RAM total: 8 131 768 KiB (~7,8 GiB).
- Engine: 0.1.0.

## Limitações

- Dataset maior que a **RAM física** (critério completo de ENT-076) **não foi medido**: o host tem
  ~7,8 GiB de RAM e o workload medido é de 171 MB. Um dataset acima da RAM exigiria cerca de 8 GB
  de CSV e cerca de 45 minutos por repetição neste hardware; essa execução ainda não foi feita.
- O pico de memória é o maior worker do processo de benchmark, acumulado sobre as repetições.
- `RLIMIT_AS` limita memória virtual; o RSS medido é menor que o limite configurado.
- O workload depende de o modelo fazer streaming. Um modelo que carrega todas as linhas em
  memória excede o teto, e o benchmark reporta isso em `within_memory_limit`.
- Não há baseline de comparação entre versões nem latências p50/p95/p99 nesta medição.
- Os resultados valem para o hardware acima; outros hosts devem repetir o procedimento.

## Reprodução verificada

Em 2026-11-15 o procedimento abaixo foi executado a partir de uma cópia de `examples/benchmark-stream`
(E-131): venv com `--without-pip`, `dowai benchmark models/stream_count.py --repeat 1 --json`.
Resultado: `SUCCEEDED`, 54 881 ms, pico de RSS 20 548 KiB, `within_memory_limit: true`.

## Procedimento para reproduzir

1. Criar o venv do projeto: `python3 -m venv --without-pip .venv` (o workload não usa pacotes; `--without-pip` evita depender de `ensurepip`), e apontar `python.interpreter` no
   `dowai_project.json` para `.venv/bin/python`.
2. Executar `dowai benchmark models/stream_count.py --repeat 3 --json`.
3. Registrar o relatório, a versão do engine e o hardware do host.
