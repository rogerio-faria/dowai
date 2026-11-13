# Plugin oficial `sftp` (v1.0.0) — código entregue, testes adiados

Transferência de arquivos entre o projeto e um servidor SFTP (ENT-113, D-117). Para usar, copie
esta pasta para `<projeto>/plugins/sftp/` e instale `requirements.txt` no ambiente do run.
Exige `plugins.allow_network = true`, `plugins.allow_filesystem = true` e
`plugins.allow_secrets = true`.

**Status:** código pronto e compilado. Os testes contra servidor SFTP ainda não foram executados,
por decisão do usuário (D-117). Não há validação real de conexão, autenticação ou transferência.

## Métodos

| Alvo | Função | Uso |
|---|---|---|
| `sftp.download` | `sftp:download` | Baixa um arquivo remoto para o projeto, com escrita atômica. |
| `sftp.upload` | `sftp:upload` | Envia um arquivo do projeto ao servidor, renomeando ao final. |

## Parâmetros

- `base_dir` (obrigatório): raiz local permitida.
- `host`, `port` (padrão 22), `known_hosts` (caminho local relativo a `base_dir`, obrigatório).
- `remote_path` (absoluto no servidor), `local_path` (relativo a `base_dir`).
- `if_exists`: `replace` (padrão) ou `fail`.
- `timeout_seconds` (padrão 30).
- Segredo `sftp_credentials` (JSON): `{"username": "...", "password": "..."}` ou
  `{"username": "...", "private_key": "<PEM>"}`. Fornecido por `DOWAI_SECRET_SFTP_CREDENTIALS`.

## Regras

- Verificação de host obrigatória pelo `known_hosts`. Host desconhecido ou chave divergente é
  recusado (`DOWAI_SFTP_HOST_KEY_FAILED`).
- Sem agente SSH e sem chaves padrão do usuário.
- Caminhos locais sem `..`, symlink ou caminho absoluto (mesma regra do plugin `files`).
- Caminhos remotos absolutos, sem `..`.
- Falha na renomeação do envio sai com código 13 (`DOWAI_SFTP_UNCERTAIN`): o efeito pode ter
  sido aplicado e a execução não deve ser repetida sem verificação.
- Mensagens de erro têm códigos estáveis e não incluem credenciais, chaves nem caminhos remotos.

## Dependência

`paramiko==5.1.0` (`requirements.txt`). O lockfile com hashes (ENT-046) será gerado na fase de
testes; até lá a instalação é apenas declarada.
