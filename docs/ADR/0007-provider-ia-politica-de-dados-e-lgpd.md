# ADR 0007: Provider de IA, política de dados, LGPD e versão OpenLineage

- Status: **Accepted** — decisões do usuário registradas em `_gestao/DECISOES.md` (D-092, 2026-11-13).
- Date: 2026-11-13
- Decide: D10 (`plano_mestre.md` §18: proveniência, retenção e envio de dados à IA) e D11 (versão OpenLineage/facets fixadas).
- Referências: `plano_mestre.md` §11.1, §11.3, §11.4; ADR 0005; `docs/CONFIGURATION.md` (`ai.profiles`, `lineage.*`).
- Tarefa: ENT-005.

## Contexto

O plano prevê IA opt-in (`env.ia`, sugestões de AutoFix, documentação e lineage assistidos por IA) e eventos de lineage compatíveis com OpenLineage. Ambos tocam dados potencialmente pessoais e segredos, por isso precisam de política explícita antes de M5.

## Decisão

### 1. Provider e configuração

- Provider padrão: **OpenAI**. Outros providers só entram por perfil explícito em `ai.profiles`.
- URL, modelo e token são **parâmetros de configuração**. Nenhum token, chave ou segredo é gravado em arquivo versionado; a credencial é referenciada pelas chaves `*_env` (variável de ambiente) ou `*_ref` (gerenciador de segredos), conforme `docs/CONFIGURATION.md`; chaves como `api_key` com valor literal são recusadas com `DOWAI_CONFIG_INVALID`.
- Sem perfil de IA configurado, o núcleo SQL/Python/plugins permanece funcional e nenhuma chamada externa ocorre.

### 2. Política default de envio de dados à IA (D10)

Por default, **nada** é enviado a provider de IA além do próprio pedido do usuário, e mesmo assim somente com opt-in explícito por projeto ou comando:

| Categoria | Enviado por default? | Regra |
|---|---|---|
| Linhas de dados (resultados de consulta, amostras, `DataFrame`s) | **Não** | Proibido no default; liberação exige configuração específica e aviso registrado em AUDIT. |
| Segredos (tokens, senhas, URLs com credencial, conteúdo de `.env`) | **Não** | Sempre removidos e redigidos antes de qualquer chamada. |
| SQL do modelo | Somente sob opt-in de IA | Redação de literais e identificadores sensíveis antes do envio. |
| Mensagens de erro | Somente sob opt-in de IA | Saneadas (credenciais, caminhos absolutos fora do projeto, valores de linha). |
| Metadados (nomes de tabelas/colunas, tipos) | Somente sob opt-in de IA | Enviados apenas se marcados no perfil. |

- Respostas de IA são **sugestões não confiáveis**: nenhuma alteração em código, SQL, configuração ou execução ocorre sem revisão/autorização explícita (§11.1, §11.4).
- Falha ou indisponibilidade do provider não corrompe estado de execução.

### 3. Retenção e proveniência

- Cada chamada registra, em trilha AUDIT: provider, modelo, finalidade, campos enviados (categoria, sem conteúdo), tamanho, tokens/custo e hash do payload.
- Payloads completos **não são persistidos** por default. Se a configuração exigir cópia para auditoria, ela deve ser local, criptografada pelo ambiente do usuário e com retenção configurável; essa opção fica para tarefa futura.
- Respostas de IA são gravadas como sugestão versionada (`*.suggested.sql`, documentação gerada), marcadas com método e confiança.

### 4. LGPD

- Princípios aplicados: finalidade (apenas geração de sugestões e documentação), necessidade (mínimo de dados; linhas excluídas por default), segurança (redação e segredos fora de arquivos versionados) e transparência (AUDIT e documentação do que foi enviado).
- O provider OpenAI pode processar dados fora do Brasil. Por isso, **transferência internacional** e **base legal** dependem de avaliação do controlador de cada projeto. Este ADR não substitui parecer jurídico. A limitação deve ser declarada em `docs/SECURITY.md` na tarefa ENT-082 (pendente).
- Dados pessoais reais de produção não devem ser enviados a IA em nenhum cenário sem decisão específica registrada.

### 5. Versão OpenLineage e facets (D11)

- Versão fixada: **OpenLineage 1.55.0** (release de 2026-10-30), cujo schema core tem `$id` `https://openlineage.io/spec/2-0-2/OpenLineage.json`.
- Cópia do schema em `specs/openlineage/OpenLineage.json`, SHA-256 `69f68bee00b9beac88a87059c0102410e7bb05f3f43c46d02a0409831eceb0d2`.
- Fixtures em `specs/fixtures/openlineage/` (válidas e inválidas) são validadas por `src/dowai-core/tests/openlineage_contract.rs`.
- Facets customizadas DOWAI usam namespace próprio (`dowai_*`, `_schemaURL` `urn:dowai:facets:...`) e nunca substituem facets oficiais.
- Dependência declarada, execução observada e inferência de IA são registradas separadamente (§11.3).
- Atualização de versão exige novo ADR ou registro em `DECISOES.md`, novo hash e revisão das fixtures.

## Consequências

- ENT-068 (gateway de IA opt-in) e ENT-073 (lineage) passam a ter política e versão fixadas.
- A implementação de envio de dados fica proibida no default até que a categoria correspondente tenha teste de redação.
- Não há nesta fase chamada real a provider de IA nem validação de conformidade LGPD; ambas são limitações registradas.
