# Plugin oficial `email` (v1.0.0)

Plugin mínimo de e-mail por SMTP (plano §9.5, ENT-059). Para usar, copie esta pasta para
`<projeto>/plugins/email/` (escopo `project`) ou `<workspace>/plugins/email/` (escopo `global`).

## Métodos

| Alvo | Função | Uso |
|---|---|---|
| `email.send` | `sender:send` | Uma mensagem para um ou mais destinatários, com template e anexos opcionais. |
| `email.send_batch` | `sender:send_batch` | Até 100 mensagens em uma sessão SMTP, enviadas em ordem. |

Toda chamada exige `idempotency_key`. Reenviar a mesma chave nunca envia de novo: após sucesso
devolve `DUPLICATE`; após falha ou resultado incerto a chave fica bloqueada e a execução é recusada
(`DOWAI_PLUGIN_IDEMPOTENCY_FAILED` ou `DOWAI_PLUGIN_IDEMPOTENCY_UNCERTAIN`). Para tentar de novo,
use outra chave, depois de verificar que a mensagem não chegou.

## Parâmetros principais

- Comuns: `sender` (remetente), `host`, `port` (padrão 587), `tls` (`starttls` padrão, `ssl` ou `none`),
  `timeout_seconds` (1 a 60, padrão 10).
- `send`: `to` (texto ou lista de 1 a 100), `subject` (linha única, até 200 caracteres), `message`
  (texto), `template` com `context` (objeto), `attachments` (lista de caminhos), `attachments_dir`.
- `send_batch`: `messages` (lista; cada item tem `to`, `subject`, `message` ou `template`/`context`,
  `attachments`) e `attachments_dir`.

Template usa `string.Template`: `$nome` ou `${nome}`. Variável ausente vira
`DOWAI_EMAIL_TEMPLATE_INVALID` antes de conectar.

## Credenciais (segredo `mail_credentials`)

O valor é um JSON com `username` e `password`. O host lê a variável de ambiente
`DOWAI_SECRET_MAIL_CREDENTIALS` e entrega o valor ao plugin pelo stdin, somente quando:

1. o manifesto declara `secrets: ["mail_credentials"]` (já declarado); e
2. a política tem `plugins.allow_secrets = true`.

Sem credenciais o plugin recusa com `DOWAI_EMAIL_CREDENTIALS_MISSING`, antes de conectar. O valor
nunca aparece em resultado, erro, AUDIT ou log. O JSON nunca vai em argumentos de linha de comando
nem em arquivos do projeto.

## Regras

- `tls: none` só é aceito para `127.0.0.1`, `localhost` ou `::1` (uso em testes).
- Cabeçalhos com quebra de linha (assunto, remetente, destinatário) são recusados.
- Anexos são lidos somente dentro de `attachments_dir`. Caminho absoluto, `..` e symlink são
  recusados. Limite de 10 MiB por anexo e de 10 anexos por mensagem.
- Recusa do servidor antes do envio do conteúdo (`DOWAI_EMAIL_REJECTED`, `DOWAI_EMAIL_CONNECTION_FAILED`)
  é falha definitiva. Queda depois do envio do conteúdo (`DOWAI_EMAIL_UNCERTAIN`) vira resultado
  incerto: a chave fica `UNCERTAIN` e nunca é reexecutada.

## Limitações

- Somente SMTP. Envio por API de provedor não foi implementado.
- Lote: se uma mensagem falhar no meio do lote, o plugin não informa quais já foram aceitas. Nesse
  caso a chave fica bloqueada e as mensagens devem ser conferidas manualmente. Para envios
  críticos, prefira uma mensagem por chave.
- Os erros do plugin chegam ao host como `DOWAI_PLUGIN_FAILED`, sem o código interno
  `DOWAI_EMAIL_*` (o worker não transporta códigos de dentro do Python).
- Bloqueio de rede e de arquivos é cooperativo (hook do worker), não sandbox de SO (ENT-108).

## Testes

- `python3 -I -m unittest discover -s tests/plugins/email -p "test_*.py"` (gateway SMTP falso em loopback).
- `cargo test -p dowai-runtime --test plugin_email` (execução pelo worker real, deduplicação e incerteza).
