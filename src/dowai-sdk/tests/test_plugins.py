"""ENT-106/ENT-107: `env.plugins.run("ns.m", **kwargs)` and `env.plugins.ns.m(**kwargs)` take the same
execution path (`dowai plugins run`).

Uses an explicit fake backend, so no engine is needed. The client-level argument building is in
test_client.py; the byte-for-byte comparison with the real CLI is in test_cli_parity.py.
"""

import sys
import unittest
from pathlib import Path

SDK_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SDK_ROOT / "src"))

from dowai import DowaiExecutionError, Env  # noqa: E402


class RecordingBackend:
    def __init__(self, error=None):
        self.resolved = []
        self.runs = []
        self._error = error

    def plugin_resolve(self, target):
        self.resolved.append(target)
        return {"target": target, "entrypoint": "sender:send", "override": None}

    def plugin_run(self, target, input=None, *, idempotency_key=None):
        self.runs.append((target, input, idempotency_key))
        if self._error is not None:
            raise self._error
        return {"status": "SUCCEEDED", "target": target, "result": {"echo": input}}


class PluginCallTests(unittest.TestCase):
    def test_both_forms_execute_through_one_path(self):
        backend = RecordingBackend()
        env = Env(backend=backend)
        by_run = env.plugins.run("email.send", to="a@example.com")
        by_attribute = env.plugins.email.send(to="a@example.com")
        self.assertEqual(by_run, by_attribute)
        self.assertEqual(
            backend.runs,
            [("email.send", {"to": "a@example.com"}, None)] * 2,
        )

    def test_call_without_arguments_executes_with_an_empty_object(self):
        backend = RecordingBackend()
        Env(backend=backend).plugins.alerts.threshold()
        self.assertEqual(backend.runs, [("alerts.threshold", {}, None)])

    def test_keyword_arguments_are_no_longer_refused_and_never_dropped(self):
        backend = RecordingBackend()
        result = Env(backend=backend).plugins.email.send(to="a@example.com", n=2)
        self.assertEqual(result["result"], {"echo": {"to": "a@example.com", "n": 2}})
        self.assertEqual(backend.runs[0][1], {"to": "a@example.com", "n": 2})

    def test_idempotency_key_is_forwarded_and_is_not_part_of_the_input(self):
        backend = RecordingBackend()
        Env(backend=backend).plugins.run("email.send", idempotency_key="alert-1", to="x")
        self.assertEqual(backend.runs, [("email.send", {"to": "x"}, "alert-1")])

    def test_resolve_does_not_execute(self):
        backend = RecordingBackend()
        resolution = Env(backend=backend).plugins.resolve("email.send")
        self.assertEqual(resolution["entrypoint"], "sender:send")
        self.assertEqual(backend.runs, [])

    def test_arguments_that_are_not_json_are_rejected_before_the_engine(self):
        backend = RecordingBackend()
        with self.assertRaises(ValueError):
            Env(backend=backend).plugins.email.send(payload={1, 2})
        self.assertEqual(backend.runs, [])

    def test_engine_refusal_propagates_with_its_stable_code(self):
        refusal = DowaiExecutionError("DOWAI_CONFIG_INVALID", "método delete não declarado")
        with self.assertRaises(DowaiExecutionError) as context:
            Env(backend=RecordingBackend(error=refusal)).plugins.run("email.delete")
        self.assertEqual(context.exception.code, "DOWAI_CONFIG_INVALID")

    def test_malformed_targets_are_rejected_before_reaching_the_engine(self):
        backend = RecordingBackend()
        env = Env(backend=backend)
        for target in ["email", ".send", "email.", "email send", "--json", 7]:
            with self.subTest(target=target), self.assertRaises(ValueError):
                env.plugins.run(target)
        self.assertEqual(backend.runs, [])

    def test_unbound_env_reports_the_runtime_error_only_when_called(self):
        env = Env()
        facade = env.plugins  # building the facade touches no runtime
        with self.assertRaises(DowaiExecutionError) as context:
            facade.run("email.send")
        self.assertEqual(context.exception.code, "DOWAI_RUNTIME_UNBOUND")

    def test_private_names_are_not_plugin_namespaces(self):
        with self.assertRaises(AttributeError):
            _ = Env(backend=RecordingBackend()).plugins._unknown


if __name__ == "__main__":
    unittest.main()
