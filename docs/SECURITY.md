# Segurança do DOWAI — threat model inicial

Documento vinculado à tarefa ENT-011 e ao `plano_mestre.md` §12.3 (itens 1 a 9), §13 e §20.1. Descreve o estado **implementado em 2026-11-12**. Controles marcados como "pendente" não devem ser tratados como garantias.

## 1. Escopo e ativos

Ativos protegidos:

- credenciais de bancos e serviços (perfis de conexão, segredos referenciados);
- dados lidos e escritos pelos pipelines;
- arquivos do projeto e do workspace, incluindo modelos SQL e Python;
- trilha AUDIT e logs;
- disponibilidade da máquina onde o DOWAI roda.

Atores considerados:

- usuário que escreve e executa o projeto (confiável por padrão, mas pode cometer erros);
- autor de código Python ou manifesto de plugin, quando diferente do operador (não confiável por padrão);
- processo de outro usuário local na mesma máquina;
- conteúdo externo (respostas de APIs, arquivos de entrada) tratado como dado não confiável.

Fora do escopo de garantia: código malicioso executado pelo próprio operador com as permissões dele, e comprometimento do sistema operacional.

## 2. Limite de sandbox de Python (§12.3 item 8)

**O código Python de modelos roda com os privilégios do usuário que executa o DOWAI.** Um venv **não** é sandbox de sistema operacional: isola dependências, não isola acesso a arquivos, rede, processos ou variáveis de ambiente que o próprio processo consiga alcançar.

Estado atual, conforme `docs/CONFIGURATION.md` e `docs/ADR/0003`:

- O worker Python é um processo supervisionado, com IPC NDJSON versionado por `stdin`/`stdout`, sem porta de rede, socket nomeado ou token.
- O worker herda apenas uma allowlist de variáveis de ambiente do runtime, não o ambiente completo do host.
- Não há isolamento de filesystem, rede ou syscalls. O código do modelo pode, em tese, ler arquivos e abrir conexões.
- Não há sandbox de SO (namespaces, seccomp, AppContainer, Job Object de restrição de acesso). Job Objects no Windows são usados para encerramento da árvore de processos, não para restringir acesso.
- Em Unix, processos filhos podem sair do grupo de processos supervisionado. **Encerramento por timeout não é fronteira de segurança.**
- Pendente: enforcement de limites de CPU e memória (ENT-038, ADR 0005 D06).

Consequência: o DOWAI deve ser usado apenas com código Python confiável. Não há garantia de contenção de código malicioso.

### 2.1 Sandbox de plugins: controle cooperativo (ENT-108, decisão D-092 opção 1)

Decisão registrada pelo usuário: **não** há sandbox de sistema operacional para plugins nesta versão. Nenhuma ferramenta de isolamento de sistema (ex.: `bubblewrap`) é instalada ou exigida.

O controle aplicado é cooperativo e limitado ao interpretador Python:

- O worker de plugin (`src/dowai-runtime/src/plugin_exec.rs`) registra um hook `sys.addaudithook`, que recusa criação de processo e, conforme a permissão do plugin, rede (`plugins.allow_network`) e escrita em arquivos (`plugins.allow_filesystem`).
- O hook **não é fronteira de segurança**. Código nativo (extensões C, `ctypes`) ou chamadas que não emitam eventos de auditoria podem contorná-lo.
- Não há isolamento de memória, syscalls, filesystem ou rede no nível do sistema operacional para plugins.

Restrição operacional (obrigatória até decisão diferente registrada em `DECISOES.md`):

- Plugins só devem ser executados quando forem **código confiável**, isto é, produzido ou revisado pelo próprio operador/equipe do workspace.
- Plugins de terceiros não confiáveis não devem ser instalados nem executados com a configuração atual.
- Alternativas que exigem decisão específica, não adotadas: sandbox de SO no Linux (namespaces, seccomp, `bubblewrap`), sandbox de SO no Windows (job objects restritivos, AppContainer) e recusa de plugins sem sandbox.

## 3. Princípio do menor privilégio (§12.3 item 1)

Estado atual:

- Escrita externa é gateada por `security.allow_external_writes`. Projetos Python-only também passam por esse controle, porque o código pode produzir efeitos colaterais arbitrários.
- Política de pacotes Python é declarativa (`python.package_policy`). Valores diferentes de `declared_and_authorized` são rejeitados até a instalação autorizada existir.
- Pendente: permissões granulares de rede, filesystem, banco e segredos por plugin (ENT-056) e perfis de conexão TLS e timeouts (ENT-030).

## 4. Segredos (§12.3 item 2)

Estado atual:

- Perfis aceitam `password` literal ou `{{ env_var('NOME') }}`. Prefira variáveis de ambiente: o valor literal fica em texto no `profiles.json`. O `init` adiciona o arquivo ao `.gitignore`, mas isso não remove arquivos já rastreados pelo Git.
- Migração de configuração legada recusa literais de segredo (`password`, `secret`, `token`, `api_key`, `apikey`) e exige referência `*_env` ou gerenciador de segredos (`src/dowai-core/src/config.rs`).
- O design do IPC impede que o segredo passe por argv, ambiente ou log do worker (ADR 0003, D05). A implementação de `connection_open` e o teste de não vazamento estão pendentes (ENT-043).
- Pendente: mascaramento sistemático em `config show` (ENT-013), redação em logs JSONL (ENT-065) e teste de vazamento em erros e planos JSON.

Regra vigente: nenhum segredo real em Git, erro, log, prompt, plano JSON ou lineage. Não há segredo de exemplo com valor real no repositório.

## 5. Caminhos, symlinks e fronteira do projeto (§12.3 item 3)

- Caminhos de projeto e workspace são resolvidos e verificados em planejamento, sessões e runs.
- Escapes de fronteira devem ser recusados sem allowlist administrativa.
- **Pendente:** validação completa de symlinks e traversal, e proteção contra alteração do arquivo entre validação e execução (ENT-024, ENT-018). Até essas tarefas serem concluídas, não há garantia de que um symlink dentro do projeto não aponte para fora dele.

## 6. Autorizações explícitas (§12.3 item 4)

- Escrita externa, operação destrutiva, acesso a rede e instalação de pacotes exigem autorização declarada. Não existe default permissivo para essas ações.
- Política administrativa não pode ser afrouxada por configuração de projeto.
- Ações sensíveis devem ser registradas no AUDIT (§12.3 item 9), mesmo quando o usuário captura a exceção (ENT-064).

## 7. Dependências e supply chain (§12.3 item 5)

- Dependências Rust usam `Cargo.lock` e `--locked` nos comandos de build e teste.
- O SDK Python (`src/dowai-sdk/pyproject.toml`) não declara dependências de runtime (`dependencies = []`), o que reduz a superfície de supply chain do próprio SDK.
- Pendente: auditoria de vulnerabilidades, licenças e SBOM (ENT-074). Pendente: lockfile e verificação de divergência de venv de projeto (ENT-046).

## 8. Cancelamento e limpeza (§12.3 item 6)

- Timeout encerra o worker Python. No Windows, a árvore é encerrada com Job Objects. Em Unix, o encerramento usa grupo de processos, mas processos destacados podem escapar.
- Arquivos temporários e estados incertos não são limpos de forma completamente verificada (ENT-062, ENT-067).
- Não há afirmação de sandbox completo. Testes específicos por SO são pendentes (ENT-039).

## 9. Testes de segurança (§12.3 item 7)

Estado atual, verificado por testes existentes:

- Templates SQL: apenas o subconjunto allowlist é aceito; expressões fora dele são rejeitadas (ADR 0004, `template_contract.rs`). Não há interpolação de valor de variável no SQL.
- Rejeição de segredos literais em migração de configuração (`config_legacy_formats.rs`).
- Contratos de schema JSON com fixtures negativas (`schema_contracts.rs`).

Pendentes (ENT-075, suíte de segurança): injeção SQL em identificadores e valores, vazamento de segredos em logs e erros, traversal, alteração entre validação e execução, desvio de perfil e logs sensíveis.

## 10. Divulgação responsável

- Canal de contato para reportar vulnerabilidades: **pendência do usuário (P-08)**. Nenhum endereço foi definido nesta versão, e nenhum contato foi inventado.
- Até a definição, vulnerabilidades devem ser reportadas ao responsável pelo projeto por canal privado, sem publicação em issues ou logs.
- Prazos de resposta e divulgação serão definidos junto com o canal.

## 11. Riscos residuais principais

| Risco | Situação |
|---|---|
| Código Python malicioso executa com privilégios do usuário | Aceito como limitação; mitigado apenas por uso de código confiável |
| Symlink dentro do projeto aponta para fora | Aberto até ENT-024 |
| Limites de CPU/memória não aplicados | Aberto até ENT-038; estado reportado por ADR 0005 |
| Vazamento de segredo em log ou erro | Parcialmente mitigado; testes pendentes (ENT-043, ENT-065, ENT-075) |
| Efeito externo duplicado em retry | Mitigado por contrato (ADR 0005 D08); outbox pendente (ENT-051) |
| Plugin de terceiros malicioso | Aberto até plugins (M4) e permissões granulares (ENT-056) |
