"""ENT-106: `env.plugins.run("ns.m")` and `env.plugins.ns.m()` take the same resolution path.

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
    def __init__(self, answer=None, error=None):
        self.targets = []
        self._answer = answer
        self._error = error

    def plugin_resolve(self, target):
        self.targets.append(target)
        if self._error is not None:
            raise self._error
        return {"target": target, "entrypoint": "sender:send", "override": None}


class PluginCallTests(unittest.TestCase):
    def test_both_forms_resolve_to_the_same_result_through_one_path(self):
        backend = RecordingBackend()
        env = Env(backend=backend)
        by_run = env.plugins.run("email.send")
        by_attribute = env.plugins.email.send()
        self.assertEqual(by_run, by_attribute)
        self.assertEqual(backend.targets, ["email.send", "email.send"])

    def test_nested_namespace_names_map_to_the_dotted_target(self):
        backend = RecordingBackend()
        Env(backend=backend).plugins.alerts.threshold()
        self.assertEqual(backend.targets, ["alerts.threshold"])

    def test_arguments_are_refused_after_resolution_never_dropped(self):
        backend = RecordingBackend()
        with self.assertRaises(DowaiExecutionError) as context:
            Env(backend=backend).plugins.email.send(to="a@example.com")
        self.assertEqual(context.exception.code, "DOWAI_PLUGIN_EXECUTION_UNAVAILABLE")
        self.assertEqual(backend.targets, ["email.send"], "resolution happens before the refusal")

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
        self.assertEqual(backend.targets, [])

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
