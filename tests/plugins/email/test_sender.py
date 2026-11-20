"""Testes do plugin `email` (ENT-059) com gateway SMTP falso em loopback.

Execução: python3 -I -m unittest discover -s tests/plugins/email -p "test_*.py"
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
sys.path.insert(0, str(ROOT / "plugins" / "email"))

import sender  # noqa: E402

FAKE = HERE / "fake_smtp.py"
CREDS = {"mail_credentials": json.dumps({"username": "robo", "password": "senha-de-teste"})}


class FakeGateway:
    def __init__(self, log):
        self.log = log
        self.trace = log.with_suffix(".trace")
        self.proc = subprocess.Popen(
            [sys.executable, "-I", str(FAKE), str(log), "0", str(self.trace)],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        line = self.proc.stdout.readline().strip()
        self.port = int(line.removeprefix("PORT "))

    def records(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()]

    def close(self):
        self.proc.kill()
        self.proc.wait()


class EmailSenderTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.gateway = FakeGateway(Path(cls.tmp.name) / "smtp.jsonl")

    @classmethod
    def tearDownClass(cls):
        cls.gateway.close()
        cls.tmp.cleanup()

    def setUp(self):
        self.attachments = Path(tempfile.mkdtemp(dir=self.tmp.name))
        (self.attachments / "dados.csv").write_text("a,b\n1,2\n", encoding="utf-8")
        self.base = dict(
            sender="dowai@example.org",
            host="127.0.0.1",
            port=self.gateway.port,
            tls="none",
            timeout_seconds=5,
            secrets=CREDS,
            attachments_dir=str(self.attachments),
        )

    def send(self, **overrides):
        args = dict(self.base, to="ops@example.org", subject="Alerta", message="oi")
        args.update(overrides)
        return sender.send(**args)

    def assert_refused(self, code, **overrides):
        with self.assertRaises(sender.EmailError) as context:
            self.send(**overrides)
        self.assertEqual(context.exception.code, code)
        return context.exception

    # Entrega e deduplicação de envio são do host; aqui conferimos o que o plugin entrega.

    def test_template_renders_and_sends_one_message(self):
        before = len(self.gateway.records())
        result = self.send(
            message="ignorado quando há template",
            template="Olá $nome, limite $limite",
            context={"nome": "Ana", "limite": "15%"},
        )
        self.assertEqual(result, {"messages": 1, "recipients": 1})
        record = self.gateway.records()[before]
        self.assertEqual(record["subject"], "Alerta")
        self.assertEqual(record["to"], ["ops@example.org"])

    def test_loopback_send_does_not_resolve_local_hostname(self):
        with patch("socket.getfqdn", side_effect=AssertionError("unexpected hostname lookup")):
            result = self.send()
        self.assertEqual(result, {"messages": 1, "recipients": 1})
        trace = self.gateway.trace.read_text(encoding="utf-8")
        self.assertIn("CONNECTED\nEHLO\nAUTH\n", trace)
        self.assertNotIn("senha-de-teste", trace)

    def test_missing_template_variable_is_refused_without_sending(self):
        before = len(self.gateway.records())
        self.assert_refused(
            "DOWAI_EMAIL_TEMPLATE_INVALID", template="Olá $nome", context={}
        )
        self.assertEqual(len(self.gateway.records()), before)

    def test_header_injection_is_refused(self):
        self.assert_refused("DOWAI_EMAIL_INPUT_INVALID", subject="Oi\r\nBcc: x@example.org")
        self.assert_refused("DOWAI_EMAIL_INPUT_INVALID", to=["a@example.org\nBcc: b@example.org"])

    def test_attachment_outside_directory_is_refused(self):
        self.assert_refused("DOWAI_EMAIL_ATTACHMENT_INVALID", attachments=["../etc/passwd"])
        self.assert_refused("DOWAI_EMAIL_ATTACHMENT_INVALID", attachments=["/etc/passwd"])

    def test_symlink_attachment_is_refused(self):
        outside = Path(self.tmp.name) / "fora.txt"
        outside.write_text("fora", encoding="utf-8")
        link = self.attachments / "link.txt"
        if link.exists() or link.is_symlink():
            link.unlink()
        os.symlink(outside, link)
        self.assert_refused("DOWAI_EMAIL_ATTACHMENT_INVALID", attachments=["link.txt"])

    def test_attachment_requires_attachments_dir(self):
        self.assert_refused("DOWAI_EMAIL_INPUT_INVALID", attachments=["dados.csv"], attachments_dir=None)

    def test_attachment_is_sent(self):
        before = len(self.gateway.records())
        self.send(attachments=["dados.csv"])
        record = self.gateway.records()[before]
        self.assertGreater(record["bytes"], 20)

    def test_tls_none_is_refused_for_non_loopback_host(self):
        self.assert_refused("DOWAI_EMAIL_TLS_REQUIRED", host="mail.example.org")

    def test_unknown_tls_mode_is_refused(self):
        self.assert_refused("DOWAI_EMAIL_INPUT_INVALID", tls="plain")

    def test_missing_credentials_are_refused(self):
        self.assert_refused("DOWAI_EMAIL_CREDENTIALS_MISSING", secrets={})

    def test_invalid_credentials_do_not_echo_the_value(self):
        error = self.assert_refused(
            "DOWAI_EMAIL_CREDENTIALS_INVALID", secrets={"mail_credentials": "{\"username\": 1}"}
        )
        self.assertNotIn("username", str(error))

    def test_wrong_credentials_fail_without_leaking_them(self):
        error = self.assert_refused(
            "DOWAI_EMAIL_CONNECTION_FAILED",
            secrets={"mail_credentials": json.dumps({"username": "", "password": "segredo-x"})},
        )
        self.assertNotIn("segredo-x", str(error))

    def test_batch_sends_all_messages_in_one_session(self):
        before = len(self.gateway.records())
        result = sender.send_batch(
            sender="dowai@example.org",
            host="127.0.0.1",
            port=self.gateway.port,
            tls="none",
            timeout_seconds=5,
            secrets=CREDS,
            attachments_dir=str(self.attachments),
            messages=[
                {"to": "a@example.org", "subject": "Um", "message": "1"},
                {"to": ["b@example.org", "c@example.org"], "subject": "Dois", "message": "2"},
            ],
        )
        self.assertEqual(result, {"messages": 2, "recipients": 3})
        self.assertEqual(len(self.gateway.records()) - before, 2)

    def test_batch_with_invalid_message_sends_nothing(self):
        before = len(self.gateway.records())
        with self.assertRaises(sender.EmailError):
            sender.send_batch(
                sender="dowai@example.org",
                host="127.0.0.1",
                port=self.gateway.port,
                tls="none",
                secrets=CREDS,
                messages=[
                    {"to": "ok@example.org", "subject": "Ok", "message": "1"},
                    {"to": "", "subject": "Ruim", "message": "2"},
                ],
            )
        self.assertEqual(len(self.gateway.records()), before)

    def test_server_refusal_before_data_is_definitive(self):
        error = self.assert_refused("DOWAI_EMAIL_REJECTED", to="reject@example.org")
        self.assertFalse(error.dowai_exit_code == sender.EXIT_UNCERTAIN)
        self.assertEqual(error.dowai_exit_code, 10)

    def test_loss_after_data_is_uncertain_with_exit_13(self):
        error = self.assert_refused("DOWAI_EMAIL_UNCERTAIN", to="drop@example.org")
        self.assertEqual(error.dowai_exit_code, sender.EXIT_UNCERTAIN)

    def test_free_port_has_no_gateway(self):
        # Garante que a recusa de conexão vira código estável, sem vazar detalhe do socket.
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            closed_port = probe.getsockname()[1]
        error = self.assert_refused("DOWAI_EMAIL_CONNECTION_FAILED", port=closed_port)
        self.assertNotIn(str(closed_port), str(error))


if __name__ == "__main__":
    unittest.main()
