# Operação do DOWAI

Documento vinculado à tarefa ENT-079 e ao `plano_mestre.md` §20.1 e §16.1. Descreve o estado **implementado em 2026-11-12**. Itens marcados como "limitação" não devem ser tratados como garantias.

## 1. Instalação e build

Requisitos: toolchain Rust com Cargo. Não há versão mínima de Rust declarada até que a matriz de compatibilidade seja estabelecida (ENT-004).

```bash
cargo test --workspace --locked
cargo build --workspace --release --locked
```

O binário fica em `target/release/dowai`. Os builds usam `Cargo.lock` (`--locked`).

Limitações:
- O build de `dowai-cli` depende de OpenSSL (`openssl-sys`). Em máquinas sem os cabeçalhos de desenvolvimento do OpenSSL, o build falha na etapa de compilação do `openssl-sys`; instale o pacote de desenvolvimento do sistema ou aponte `OPENSSL_DIR`.
- Instaladores (DEB, MSI) não fazem parte desta versão (ver `_gestao/PLANO.md`).
- Sem plataforma homologada além do Linux de desenvolvimento. Testes com PostgreSQL exigem as variáveis `DOWAI_TEST_PG_*`; sem elas, esses testes não rodam.

## 2. Diretório de estado

Por padrão, o estado do usuário fica em:

| Sistema | Caminho base |
|---|---|
| Linux | `$XDG_STATE_HOME/dowai/` ou `~/.local/state/dowai/` |
| Windows | `%LOCALAPPDATA%\DOWAI\state\` |

Arquivos principais:

| Arquivo | Conteúdo |
|---|---|
| `sessions.sqlite3` | Sessões e variáveis tipadas (expiram em 24 horas) |
| `runs.sqlite3` | Journal de execuções (status, tempos, códigos de erro, árvore de invocações) |
| `audit.jsonl` | Trilha de auditoria append-only por convenção |
| `logs/dowai.jsonl` | Log operacional com rotação e retenção |

Valores de variáveis, SQL renderizado, mensagens de erro e caminhos não são gravados no journal de execuções.

## 3. Sessões

```bash
dowai session start [--json]
dowai session show <session_id> [--json]   # mostra nomes de variáveis, nunca valores
dowai session end <session_id> [--json]
```

- Sessões expiram após 24 horas. A limpeza de sessões expiradas é preguiçosa: ocorre na próxima abertura do armazenamento.
- Limites iniciais: 1.000 variáveis, 64 KiB por valor e 16 MiB por sessão. Esses limites e o TTL ainda não são configuráveis.
- Escritas concorrentes na mesma variável não são sobrescritas silenciosamente. Por padrão (`sessions.conflict_policy = error`), a execução falha com `DOWAI_SESSION_CONFLICT` (exit 3) e não publica nada. Com `wait_process`, a execução aguarda o bloqueio da sessão até `sessions.wait_process_timeout_seconds`.

## 4. Execução, logs e reexecução

```bash
dowai run <model|all> [--profile NAME] [--session ID] [--vars JSON] [--yes] [--json]
dowai runs list [--limit N] [--json]        # N de 1 a 1000, padrão 20
dowai runs show <run_id> [--json]
dowai logs <run_id> [--node NODE] [--level ERROR|WARN|INFO|DEBUG|TRACE] [--json]
dowai retry <run_id> [--failed-only] [--vars JSON] [--yes] [--json]
```

- `dowai logs` mostra a árvore de invocações do journal, sem mensagens de texto livre.
- `dowai retry` reexecuta uma run `FAILED` ou `CANCELLED` como nova run. Variáveis não são gravadas no journal; `--vars` deve ser informado de novo quando a run original as usou.
- Ctrl+C ou SIGTERM (SIGHUP no Unix) cancelam a execução: a run termina `CANCELLED` com exit 130 e `DOWAI_CANCELLED`, árvores de processos Python são encerradas e nenhuma variável é publicada.
- Limitação: no Windows apenas Ctrl+C é tratado; não há teste com sinal real nessa plataforma.

## 5. Backup e restauração

**Limitação importante:** backup e restauração existem como funções da biblioteca `dowai-core` (API Rust). **Não há comando de CLI** para eles nesta versão. O procedimento abaixo usa a API Rust e os passos manuais indicados.

Funções disponíveis em `dowai_core::state_backup`:

- `backup_run_journal(source, backup_dir)`: cria backup verificado e consistente de `runs.sqlite3`. A origem é aberta somente leitura e não é migrada pelo backup.
- `restore_run_journal(backup, target)`: restaura em um destino **que não existe**; nunca sobrescreve um journal existente. Um arquivo que não é SQLite é recusado antes de criar o destino.
- `prune_run_backups(backup_dir, keep)`: mantém apenas os `keep` backups mais recentes. Retenção 0 é recusada. Arquivos que o DOWAI não criou são preservados. Não é automática.
- `list_run_backups(backup_dir)`: lista os backups.

### Procedimento de backup

1. Confirme que não há `dowai run` ativo para o mesmo estado (o journal pode estar em uso).
2. Chame `backup_run_journal(<state>/runs.sqlite3, <dir_backup>)`.
3. Guarde o `BackupReport` retornado. Mantenha a retenção com `prune_run_backups`.

### Procedimento de restauração (em ambiente de teste ou após falha)

1. Pare qualquer `dowai run` em andamento.
2. Renomeie (não apague) o `runs.sqlite3` atual, preservando-o para análise.
3. Chame `restore_run_journal(<backup>, <state>/runs.sqlite3)`. O retorno é a versão de schema restaurada; o journal é migrado para a versão atual.
4. Valide com `dowai runs list` e `dowai runs show <run_id>` em uma run conhecida.

Limitações:
- `audit.jsonl`, `sessions.sqlite3` e `logs/dowai.jsonl` **não** são cobertos por estas funções. Faça cópia desses arquivos manualmente, com o DOWAI parado.
- O backup não foi testado com processo concorrente gravando no journal.

### Testes que validam o procedimento

Executados em 2026-11-12 (ver `_gestao/EVIDENCIAS.md`):

```bash
cargo test -p dowai-core --locked --lib state_backup
```

Cobrem: backup e restauração com preservação de runs e schema atual; migração de backup de schema 1 sem perda de runs; recusa de sobrescrita; recusa de arquivo corrompido; recusa de schema não suportado; retenção.

## 6. Recuperação de estados incertos

- Na próxima execução, uma run sem registro final cujo lock esteja livre vira `FAILED` com `DOWAI_RUN_INTERRUPTED`.
- Uma escrita externa em estado `SENT` vira `UNCERTAIN` somente quando não há atividade viva (verificação por arquivo de gate). Nesse caso, não reexecute automaticamente: verifique o destino e decida manualmente.
- Checkpoint de variáveis só é publicado por commit atômico.

Limitação: a recuperação de nós individuais ainda não é incremental (não há journal de nós em andamento) e não há teste com queda real de processo do SO; as quedas são injetadas por fase em `dowai-core`.

## 7. Logs

- Log operacional: `logs/dowai.jsonl` no diretório de estado. Limites configuráveis:
  - `logging.retention_days` (padrão 90, 1 a 3650 dias);
  - `logging.rotation_max_mb` (padrão 64 MiB).
- Redação: segredos e valores de variáveis não são registrados.
- Auditoria: `audit.jsonl` registra eventos de run, nó, nome de variável, escrita externa, negação de política e invocações aninhadas, com hashes SHA-256 do modelo, da configuração efetiva e do plano. Não pode ser desabilitada pela configuração do projeto.

## 8. Upgrades

- O schema do journal de execuções (`runs.sqlite3`) é migrado automaticamente ao ser aberto. Journals de schema anterior (testado com schema 1) são migrados sem perda de runs.
- Antes de atualizar o binário, faça backup de `runs.sqlite3` e cópia de `sessions.sqlite3`, `audit.jsonl` e `logs/`.
- Versões de schema não suportadas (por exemplo, mais novas que o binário) são recusadas, sem alteração do arquivo.
- Não há política de versões suportadas declarada (ENT-004 pendente de decisão de produto).

## 9. Diagnóstico

```bash
dowai doctor [--json]
dowai doctor --check-connections [--json]
dowai config show --effective [--json]
```

- `doctor --check-connections` usa o perfil padrão e os perfis do `profiles.json`. Perfis exigem `password` literal ou `{{ env_var('NOME') }}`; TLS é obrigatório por padrão e `tls: "disabled"` só é aceito em hosts loopback.
- Segredos de perfil nunca são gravados em JSON.
- Escritas no banco são negadas sem `security.allow_external_writes`. Substituir tabela ou view exige `--yes`.

## 9.1 Execução em ondas (modo automático)

No modo automático, os nós de um run são agrupados em ondas: um nó entra numa onda quando suas dependências já foram concluídas. Os nós de uma mesma onda executam em paralelo, com no máximo `min(runtime.max_parallel_nodes, runtime.max_worker_processes)` simultâneos (ADR 0008).

- Cada nó lê o snapshot de variáveis do início da onda. As variáveis escritas pela onda são publicadas ao fim, na ordem do plano, somente se a onda não tiver conflito e todos os nós tiverem sucesso.
- Dois nós da mesma onda que escrevam a mesma variável geram `DOWAI_VARIABLE_CONFLICT` (exit 3): o run termina `FAILED` e nenhuma variável da sessão é publicada.
- Falha de um nó interrompe os nós pendentes da mesma onda e aplica `execution.stop_on_error`. Efeitos já materializados (tabelas, views e envios à outbox) não são revertidos pelo scheduler; somente as variáveis de sessão são tudo-ou-nada.
- Journal e eventos de AUDIT são gravados na ordem do plano, independentemente da ordem real de conclusão.
- O modo manual (`dowai run --manual`, `env.manual()`) executa em sequência, sem paralelismo, por contrato.
- Em runs somente SQL, todos os nós são renderizados antes da primeira instrução: uma falha de renderização interrompe o run sem executar nenhum nó. Em runs com Python ou misto, cada nó é renderizado ao iniciar a sua onda, com o snapshot dessa onda; a falha de um nó é registrada no journal e interrompe os pendentes da onda.

## 10. Erros e códigos de saída

Erros usam códigos estáveis `DOWAI_*`, e a saída `--json` vai somente para stdout. Consulte `docs/CONFIGURATION.md` para o catálogo de parâmetros e o plano mestre para a tabela de códigos de saída.
