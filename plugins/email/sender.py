"""Plugin oficial mínimo `email` (ENT-059, plano §9.5).

Entrypoints (plugin.json):
- send: uma mensagem para um ou mais destinatários, com template e anexos opcionais.
- send_batch: várias mensagens em uma única sessão SMTP, enviadas em ordem.

Regras:
- Credenciais vêm do segredo `mail_credentials` (JSON com `username` e `password`). O host
  injeta o segredo só quando o manifesto declara a permissão e a política a permite. Nunca são
  copiadas para erro, log ou resultado.
- TLS: `starttls` (padrão) ou `ssl`. `none` só para host de loopback (API falsa de teste).
- Quebra de linha em cabeçalhos é recusada (injeção de cabeçalho).
- Anexos são lidos somente de `attachments_dir`; caminho absoluto, `..` e symlink são recusados.
- Deduplicação não é feita aqui: o host registra a chave de idempotência na outbox e nunca
  reexecuta uma chave `FAILED`, `UNCERTAIN` ou já `CONFIRMED`.
- Falha antes do envio do DATA: código `DOWAI_EMAIL_*` e saída 10 (nada foi aceito pelo servidor).
  Falha depois do envio do DATA sem resposta conclusiva: saída 13, que o host trata como incerta.
  Nesse caso a chave fica `UNCERTAIN` e a mensagem deve ser verificada, não reenviada.
- `send_batch` recusa o lote inteiro antes de conectar se alguma mensagem for inválida. Falha no
  meio do envio não informa quantas mensagens foram aceitas (limitação documentada).
"""

import json
import mimetypes
import os
import smtplib
import socket
import ssl
import string
from email.message import EmailMessage

EXIT_UNCERTAIN = 13
MAX_RECIPIENTS = 100
MAX_BATCH = 100
MAX_ATTACHMENTS = 10
MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024
MAX_BODY_CHARS = 1024 * 1024
MAX_SUBJECT_CHARS = 200
TLS_MODES = ("starttls", "ssl", "none")
LOOPBACK_HOSTS = ("127.0.0.1", "localhost", "::1")


class EmailError(Exception):
    """Falha do plugin com código estável. `dowai_exit_code` diz ao worker se o efeito é incerto."""

    def __init__(self, code, message, uncertain=False):
        super().__init__(message)
        self.code = code
        self.dowai_exit_code = EXIT_UNCERTAIN if uncertain else 10


def _single_line(value, field):
    if not isinstance(value, str) or not value.strip():
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", f"{field} deve ser texto não vazio")
    if "\r" in value or "\n" in value:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", f"{field} não pode conter quebra de linha")
    return value


def _recipients(to):
    items = [to] if isinstance(to, str) else to
    if not isinstance(items, list) or not 1 <= len(items) <= MAX_RECIPIENTS:
        raise EmailError(
            "DOWAI_EMAIL_INPUT_INVALID", f"to deve ser destinatário ou lista de 1 a {MAX_RECIPIENTS}"
        )
    return [_single_line(item, "destinatário") for item in items]


def _body(item):
    template = item.get("template")
    if template is None:
        message = item.get("message", "")
        if not isinstance(message, str):
            raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "message deve ser texto")
        body = message
    else:
        context = item.get("context") or {}
        if not isinstance(template, str) or not isinstance(context, dict):
            raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "template e context devem ser texto e objeto")
        try:
            body = string.Template(template).substitute(context)
        except (KeyError, ValueError) as error:
            raise EmailError(
                "DOWAI_EMAIL_TEMPLATE_INVALID", "template com variável ausente ou inválida"
            ) from error
    if len(body) > MAX_BODY_CHARS:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "corpo acima do limite de 1 MiB")
    return body


def _attachments(items, attachments_dir):
    if not items:
        return []
    if attachments_dir is None:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "anexos exigem attachments_dir")
    if not isinstance(items, list) or len(items) > MAX_ATTACHMENTS:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", f"attachments deve ter até {MAX_ATTACHMENTS} itens")
    root = os.path.realpath(attachments_dir)
    if not os.path.isdir(root):
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "attachments_dir não é um diretório")
    result = []
    for item in items:
        if not isinstance(item, str) or not item or os.path.isabs(item):
            raise EmailError("DOWAI_EMAIL_ATTACHMENT_INVALID", "caminho de anexo inválido")
        if ".." in item.replace("\\", "/").split("/"):
            raise EmailError("DOWAI_EMAIL_ATTACHMENT_INVALID", "caminho de anexo inválido")
        candidate = os.path.join(root, item)
        real = os.path.realpath(candidate)
        if os.path.islink(candidate) or not real.startswith(root + os.sep) or not os.path.isfile(real):
            raise EmailError("DOWAI_EMAIL_ATTACHMENT_INVALID", "anexo fora de attachments_dir ou inexistente")
        if os.path.getsize(real) > MAX_ATTACHMENT_BYTES:
            raise EmailError("DOWAI_EMAIL_ATTACHMENT_INVALID", "anexo acima de 10 MiB")
        result.append((os.path.basename(item), real))
    return result


def _prepare(item, sender, attachments_dir):
    """Monta a mensagem inteira antes de qualquer conexão, para recusar lote inválido sem enviar."""
    if not isinstance(item, dict):
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "cada mensagem deve ser um objeto")
    recipients = _recipients(item.get("to"))
    subject = _single_line(item.get("subject"), "subject")
    if len(subject) > MAX_SUBJECT_CHARS:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "subject acima de 200 caracteres")
    body = _body(item)
    attachments = _attachments(item.get("attachments"), attachments_dir)
    message = EmailMessage()
    message["From"] = sender
    message["To"] = ", ".join(recipients)
    message["Subject"] = subject
    message.set_content(body)
    for name, path in attachments:
        mime = mimetypes.guess_type(name)[0] or "application/octet-stream"
        maintype, subtype = mime.split("/", 1)
        with open(path, "rb") as handle:
            message.add_attachment(handle.read(), maintype=maintype, subtype=subtype, filename=name)
    return message


def _credentials(secrets):
    raw = (secrets or {}).get("mail_credentials")
    if raw is None:
        raise EmailError("DOWAI_EMAIL_CREDENTIALS_MISSING", "segredo mail_credentials não disponível")
    try:
        value = json.loads(raw) if isinstance(raw, str) else raw
    except ValueError as error:
        raise EmailError("DOWAI_EMAIL_CREDENTIALS_INVALID", "segredo mail_credentials inválido") from error
    if not isinstance(value, dict) or not isinstance(value.get("username"), str) or not isinstance(
        value.get("password"), str
    ):
        raise EmailError("DOWAI_EMAIL_CREDENTIALS_INVALID", "segredo mail_credentials inválido")
    return value["username"], value["password"]


def _connect(host, port, tls, timeout_seconds, credentials):
    if not isinstance(host, str) or not host:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "host é obrigatório")
    if not isinstance(port, int) or not 1 <= port <= 65535:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "port fora do intervalo 1 a 65535")
    if tls not in TLS_MODES:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "tls deve ser starttls, ssl ou none")
    if tls == "none" and host not in LOOPBACK_HOSTS:
        raise EmailError("DOWAI_EMAIL_TLS_REQUIRED", "tls none só é aceito para host de loopback")
    if not isinstance(timeout_seconds, (int, float)) or not 1 <= timeout_seconds <= 60:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", "timeout_seconds fora do intervalo 1 a 60")
    context = ssl.create_default_context()
    try:
        if tls == "ssl":
            server = smtplib.SMTP_SSL(host, port, timeout=timeout_seconds, context=context)
        else:
            server = smtplib.SMTP(host, port, timeout=timeout_seconds)
            server.ehlo()
            if tls == "starttls":
                server.starttls(context=context)
                server.ehlo()
        server.login(*credentials)
    except (smtplib.SMTPException, OSError, ssl.SSLError) as error:
        raise EmailError(
            "DOWAI_EMAIL_CONNECTION_FAILED", "falha ao conectar ou autenticar no servidor SMTP"
        ) from error
    return server


def _deliver(messages, connection):
    """Envia em ordem. Erro antes do DATA é definitivo; erro de transporte depois dele é incerto."""
    server = _connect(**connection)
    try:
        for message in messages:
            try:
                server.send_message(message)
            except (smtplib.SMTPRecipientsRefused, smtplib.SMTPSenderRefused) as error:
                raise EmailError("DOWAI_EMAIL_REJECTED", "servidor recusou remetente ou destinatário") from error
            except smtplib.SMTPDataError as error:
                raise EmailError("DOWAI_EMAIL_REJECTED", "servidor recusou o conteúdo da mensagem") from error
            except (smtplib.SMTPException, OSError, socket.timeout) as error:
                raise EmailError(
                    "DOWAI_EMAIL_UNCERTAIN",
                    "falha de transporte durante o envio; a entrega é incerta",
                    uncertain=True,
                ) from error
    finally:
        try:
            server.quit()
        except (smtplib.SMTPException, OSError):
            server.close()
    recipients = sum(len(str(m["To"]).split(",")) for m in messages)
    return {"messages": len(messages), "recipients": recipients}


def send(
    sender,
    to,
    subject,
    host,
    port=587,
    tls="starttls",
    message="",
    template=None,
    context=None,
    attachments=None,
    attachments_dir=None,
    timeout_seconds=10,
    secrets=None,
):
    """Envia uma mensagem. Os argumentos são os mesmos de `send_batch`, em um único item."""
    item = {
        "to": to,
        "subject": subject,
        "message": message,
        "template": template,
        "context": context,
        "attachments": attachments,
    }
    _single_line(sender, "sender")
    prepared = _prepare(item, sender, attachments_dir)
    credentials = _credentials(secrets)
    return _deliver(
        [prepared],
        {
            "host": host,
            "port": port,
            "tls": tls,
            "timeout_seconds": timeout_seconds,
            "credentials": credentials,
        },
    )


def send_batch(
    sender,
    host,
    messages,
    port=587,
    tls="starttls",
    attachments_dir=None,
    timeout_seconds=10,
    secrets=None,
):
    """Envia `messages` (lista de objetos com `to`, `subject` e corpo) em uma sessão SMTP."""
    _single_line(sender, "sender")
    if not isinstance(messages, list) or not 1 <= len(messages) <= MAX_BATCH:
        raise EmailError("DOWAI_EMAIL_INPUT_INVALID", f"messages deve ter de 1 a {MAX_BATCH} itens")
    prepared = [_prepare(item, sender, attachments_dir) for item in messages]
    credentials = _credentials(secrets)
    return _deliver(
        prepared,
        {
            "host": host,
            "port": port,
            "tls": tls,
            "timeout_seconds": timeout_seconds,
            "credentials": credentials,
        },
    )
