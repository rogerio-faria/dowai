"""Gateway SMTP falso para os testes do plugin `email` (ENT-059). Somente loopback, sem TLS.

Uso: python3 -I fake_smtp.py <log.jsonl> [porta]   (porta 0 = automática)

Quando pronto, imprime `PORT <n>` em stdout. Cada mensagem aceita vira uma linha JSON no log
(remetente, destinatários, assunto e tamanho; nunca credenciais).

Comportamento por endereço de destinatário:
- `reject@...`: 550 no RCPT TO (recusa antes do DATA, falha definitiva).
- `drop@...`: fecha a conexão logo após receber o DATA, sem resposta (entrega incerta). A
  mensagem NÃO entra no log, porque o servidor não a confirma.

AUTH PLAIN aceita qualquer par usuário/senha não vazio.
"""

import base64
import email
import json
import socketserver
import sys
import threading


class Handler(socketserver.StreamRequestHandler):
    def send(self, line):
        self.wfile.write((line + "\r\n").encode("ascii"))
        self.wfile.flush()

    def handle(self):
        log = self.server.log_path
        self.sender = None
        self.recipients = []
        self.authed = False
        self.send("220 fake.dowai.test ESMTP ready")
        while True:
            raw = self.rfile.readline()
            if not raw:
                return
            line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
            verb = line.split(" ", 1)[0].upper()
            if verb in ("EHLO", "HELO"):
                if verb == "EHLO":
                    self.wfile.write(b"250-fake.dowai.test\r\n250 AUTH PLAIN\r\n")
                    self.wfile.flush()
                else:
                    self.send("250 fake.dowai.test")
            elif verb == "AUTH":
                parts = line.split(" ")
                try:
                    decoded = base64.b64decode(parts[2]).split(b"\0")
                    ok = len(decoded) == 3 and bool(decoded[1]) and bool(decoded[2])
                except (IndexError, ValueError):
                    ok = False
                if ok:
                    self.authed = True
                    self.send("235 authenticated")
                else:
                    self.send("535 authentication failed")
            elif verb == "MAIL":
                if not self.authed:
                    self.send("530 authentication required")
                    continue
                self.sender = line.split(":", 1)[1].strip().strip("<>")
                self.recipients = []
                self.send("250 ok")
            elif verb == "RCPT":
                address = line.split(":", 1)[1].strip().strip("<>")
                if address.startswith("reject@"):
                    self.send("550 mailbox rejected")
                else:
                    self.recipients.append(address)
                    self.send("250 ok")
            elif verb == "DATA":
                if not self.recipients:
                    self.send("503 no recipients")
                    continue
                self.send("354 end with <CRLF>.<CRLF>")
                data = self._read_data()
                if any(r.startswith("drop@") for r in self.recipients):
                    return  # sem resposta: entrega incerta para o cliente
                message = email.message_from_bytes(data)
                record = {
                    "from": self.sender,
                    "to": self.recipients,
                    "subject": message.get("Subject", ""),
                    "bytes": len(data),
                }
                with self.server.lock:
                    with open(log, "a", encoding="utf-8") as handle:
                        handle.write(json.dumps(record, ensure_ascii=False) + "\n")
                self.send("250 queued")
                self.sender, self.recipients = None, []
            elif verb in ("RSET", "NOOP"):
                self.send("250 ok")
            elif verb == "QUIT":
                self.send("221 bye")
                return
            else:
                self.send("502 command not implemented")

    def _read_data(self):
        lines = []
        while True:
            raw = self.rfile.readline()
            if not raw:
                return b""
            if raw in (b".\r\n", b".\n"):
                break
            if raw.startswith(b"."):
                raw = raw[1:]
            lines.append(raw)
        return b"".join(lines)


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    log_path = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    server = Server(("127.0.0.1", port), Handler)
    server.log_path = log_path
    server.lock = threading.Lock()
    print(f"PORT {server.server_address[1]}", flush=True)
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
