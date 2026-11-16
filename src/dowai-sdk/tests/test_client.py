"""DowaiClient argument building and result mapping, against a fake engine executable."""

import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

SDK_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SDK_ROOT / "src"))

from dowai import DowaiClient, DowaiExecutionError  # noqa: E402

# Prints one JSON object: the chosen payload plus the arguments it received.
FAKE_ENGINE = """import json, sys
arguments = sys.argv[1:]
if arguments == ["--version"]:
    print("dowai {version}")
    raise SystemExit(0)
if arguments[:1] == ["retry"] and "--failed-only" not in arguments:
    print(json.dumps({{"status": "FAILED", "error_code": "DOWAI_RUN_NOT_FOUND",
                      "message": "No run", "schema_version": 2, "details": None}}))
elif arguments[:1] == ["run"] and "broken.py" in arguments:
    print(json.dumps({{"status": "FAILED", "error_code": "DOWAI_PYTHON_FAILED",
                      "message": "Python model failed", "schema_version": 2, "details": None,
                      "run_id": "r"}}))
elif arguments[:1] == ["run"]:
    print(json.dumps({{"status": "SUCCEEDED", "schema_version": 2, "session_id": "s",
                      "run_id": "r", "invocation_id": "i", "execution_mode": "manual",
                      "duration_ms": 1, "profile": "python", "nodes": [{{"id": "ok.py"}}],
                      "warnings": [], "echo": arguments}}))
else:
    print(json.dumps({{"schema_version": 2, "echo": arguments}}))
"""


def _write_engine(directory, version="0.1.0", source=None):
    """Writes a fake `dowai` executable that runs `source` (default: FAKE_ENGINE) under Python.

    POSIX: a `dowai` file with a shebang. Windows: a `dowai.cmd` wrapper, because a bare file
    without extension is not found by `shutil.which` and cannot be started directly.
    """
    directory = Path(directory)
    if source is None:
        source = FAKE_ENGINE.format(version=version)
    script = directory / "dowai_engine.py"
    script.write_text(source)
    if os.name == "nt":
        engine = directory / "dowai.cmd"
        engine.write_text(f'@"{sys.executable}" "{script}" %*\r\n')
        return engine
    engine = directory / "dowai"
    engine.write_text(f"#!{sys.executable}\n" + source)
    engine.chmod(engine.stat().st_mode | stat.S_IXUSR)
    return engine


class FakeEngineTests(unittest.TestCase):
    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()
        engine = _write_engine(self._directory.name)
        self.client = DowaiClient(self._directory.name, binary=engine)

    def tearDown(self):
        self._directory.cleanup()

    def test_run_builds_the_same_arguments_as_the_cli(self):
        result = self.client.run(
            ["ok.py"], manual=True, profile="warehouse", variables={"batch": 7}, confirm_destructive=True
        )
        self.assertEqual(result.status, "SUCCEEDED")
        self.assertEqual(result.run_id, "r")
        self.assertEqual(result.execution_mode, "manual")
        self.assertEqual(result.nodes, ({"id": "ok.py"},))

    def test_plugin_run_builds_the_cli_arguments(self):
        payload = self.client.plugin_run(
            "email.send", {"to": "a@example.com", "n": 1}, idempotency_key="alert-1"
        )
        self.assertEqual(
            payload["echo"],
            [
                "plugins",
                "run",
                "email.send",
                "--input",
                '{"to": "a@example.com", "n": 1}',
                "--idempotency-key",
                "alert-1",
                "--json",
            ],
        )

    def test_plugin_run_without_input_or_key_sends_an_empty_object(self):
        payload = self.client.plugin_run("email.send")
        self.assertEqual(
            payload["echo"], ["plugins", "run", "email.send", "--input", "{}", "--json"]
        )

    def test_plugin_run_rejects_bad_arguments_before_running_the_engine(self):
        for target, args in [("--json", {}), ("email.send", [1]), ("email.send", {"a": {1, 2}})]:
            with self.subTest(target=target), self.assertRaises(ValueError):
                self.client.plugin_run(target, args)
        with self.assertRaises(ValueError):
            self.client.plugin_run("email.send", {}, idempotency_key="--x")

    def test_plan_passes_project_dir_and_returns_the_engine_payload(self):
        payload = self.client.plan(["ok.py"], manual=True)
        self.assertEqual(
            payload["echo"],
            ["plan", "ok.py", "--project-dir", self._directory.name, "--json", "--manual"],
        )

    def test_retry_refusal_is_a_failed_result_with_the_stable_code(self):
        result = self.client.retry("11111111-1111-4111-8111-111111111111")
        self.assertTrue(result.failed)
        self.assertEqual(result.error_code, "DOWAI_RUN_NOT_FOUND")
        self.assertEqual(result.message, "No run")

    def test_failed_run_keeps_the_run_id_the_engine_reported(self):
        result = self.client.run(["broken.py"])
        self.assertTrue(result.failed)
        self.assertEqual(result.run_id, "r")
        self.assertEqual(result.error_code, "DOWAI_PYTHON_FAILED")
        with self.assertRaises(DowaiExecutionError) as context:
            result.raise_for_error()
        self.assertEqual(context.exception.run_id, "r")

    def test_variables_are_sent_as_one_json_argument(self):
        payload = self.client.logs("11111111-1111-4111-8111-111111111111", node="ok.py")
        self.assertEqual(
            payload["echo"],
            ["logs", "11111111-1111-4111-8111-111111111111", "--level", "INFO", "--node", "ok.py", "--json"],
        )

    def test_targets_that_look_like_options_are_refused_before_running(self):
        with self.assertRaises(ValueError):
            self.client.run(["--yes"])
        with self.assertRaises(ValueError):
            self.client.retry("-x")
        with self.assertRaises(ValueError):
            self.client.run(["ok.py"], variables=["not", "a", "mapping"])

    def test_limit_is_bounded_like_the_cli(self):
        with self.assertRaises(ValueError):
            self.client.runs(0)
        with self.assertRaises(ValueError):
            self.client.runs(True)

    def test_missing_binary_is_a_stable_error(self):
        with self.assertRaises(DowaiExecutionError) as context:
            DowaiClient(self._directory.name, binary=Path(self._directory.name) / "absent")
        self.assertEqual(context.exception.code, "DOWAI_CLI_NOT_FOUND")


class EngineCompatibilityTests(unittest.TestCase):
    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()

    def tearDown(self):
        self._directory.cleanup()

    def test_same_major_and_minor_engine_is_accepted(self):
        engine = _write_engine(self._directory.name, version="0.1.7")
        client = DowaiClient(self._directory.name, binary=engine)
        self.assertEqual(client.plan(["ok.py"])["echo"][0], "plan")

    def test_engine_with_another_minor_version_is_refused_explicitly(self):
        engine = _write_engine(self._directory.name, version="0.2.0")
        with self.assertRaises(DowaiExecutionError) as context:
            DowaiClient(self._directory.name, binary=engine)
        self.assertEqual(context.exception.code, "DOWAI_VERSION_INCOMPATIBLE")
        self.assertIn("0.2.0", str(context.exception))
        self.assertIn("0.1", str(context.exception))

    def test_unparseable_version_output_is_refused(self):
        engine = _write_engine(self._directory.name, source="print('not a version')\n")
        with self.assertRaises(DowaiExecutionError) as context:
            DowaiClient(self._directory.name, binary=engine)
        self.assertEqual(context.exception.code, "DOWAI_VERSION_INCOMPATIBLE")

    def test_sdk_exposes_its_version(self):
        import dowai

        self.assertEqual(dowai.__version__, "0.1.0")


if __name__ == "__main__":
    unittest.main()
