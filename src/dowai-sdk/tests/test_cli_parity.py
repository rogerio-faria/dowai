"""AC034: the same operation through the CLI and through the Python client gives the same plan,
status, node identifiers and stable error codes, on the same project and the same journal.

Needs a built engine binary: DOWAI_BIN, or target/debug/dowai at the repository root. Skipped
otherwise. Uses a temporary user state, so it never touches the real journal or sessions.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SDK_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SDK_ROOT / "src"))

from dowai import DowaiClient, DowaiExecutionError, Env  # noqa: E402

REPOSITORY_ROOT = SDK_ROOT.parents[1]
UNKNOWN_RUN = "00000000-0000-4000-8000-000000000000"


def _engine_binary():
    name = "dowai.exe" if os.name == "nt" else "dowai"
    candidate = os.environ.get("DOWAI_BIN") or str(REPOSITORY_ROOT / "target" / "debug" / name)
    return candidate if Path(candidate).is_file() else None


@unittest.skipIf(_engine_binary() is None, "needs a built dowai binary (DOWAI_BIN)")
class CliParityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = _engine_binary()
        cls._root = tempfile.TemporaryDirectory()
        root = Path(cls._root.name)
        cls._saved = {}
        for name in ("XDG_STATE_HOME", "XDG_CONFIG_HOME", "LOCALAPPDATA", "APPDATA", "PROGRAMDATA"):
            cls._saved[name] = os.environ.get(name)
            os.environ[name] = str(root / name.lower())
        cls.project = Path(
            cls.cli_json(["init", "demo", "--workspace", str(root / "ws")], cwd=root)["project_dir"]
        )
        manifest_path = cls.project / "dowai_project.json"
        manifest = json.loads(manifest_path.read_text())
        # The policy requires the interpreter inside the project: use a project venv.
        venv = cls.project / ".venv"
        subprocess.run([sys.executable, "-m", "venv", "--without-pip", str(venv)], check=True)
        interpreter = venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        manifest["python"] = {"interpreter": str(interpreter), "timeout_seconds": 30}
        manifest.setdefault("config", {}).setdefault("security", {})["allow_external_writes"] = True
        # ENT-056: plugins que escrevem arquivo declaram `filesystem` e a política os permite.
        manifest["config"].setdefault("plugins", {})["allow_filesystem"] = True
        manifest_path.write_text(json.dumps(manifest))
        (cls.project / "models" / "ok.py").write_text(
            "def model(env):\n    env.setVar('x', 1)\n"
        )
        (cls.project / "models" / "fail.py").write_text(
            "def model(env):\n    raise RuntimeError('private detail')\n"
        )
        # ENT-106: a plugin declared in the project. Resolution must never import it.
        plugin_dir = cls.project / "plugins" / "email"
        plugin_dir.mkdir(parents=True)
        cls.plugin_sentinel = root / "plugin-imported.flag"
        (plugin_dir / "plugin.json").write_text(
            json.dumps(
                {
                    "schema_version": 2,
                    "id": "email",
                    "version": "1.0.0",
                    "entrypoints": {"send": "sender:send"},
                    "permissions": {"network": [], "filesystem": [], "secrets": []},
                    "timeout_seconds": 30,
                    "side_effects": False,
                    "idempotency_key_required": False,
                }
            )
        )
        (plugin_dir / "sender.py").write_text(
            f"open({str(cls.plugin_sentinel)!r}, 'w').write('imported')\n"
            "def send(**kwargs):\n    return None\n"
        )
        # ENT-107: plugins that are executed. `echo` is pure; `notify` has an external effect and
        # requires an idempotency key (the effect is a line appended to `effects.log`).
        cls.effects = root / "effects.log"
        cls._write_plugin("echo", "def run(**kwargs):\n    return {'echo': kwargs}\n", False)
        cls._write_plugin(
            "notify",
            f"def run(**kwargs):\n    open({str(cls.effects)!r}, 'a').write('x')\n"
            "    return {'sent': True}\n",
            True,
        )
        cls.client = DowaiClient(cls.project, binary=cls.binary)

    @classmethod
    def _write_plugin(cls, plugin_id, source, effects):
        plugin_dir = cls.project / "plugins" / plugin_id
        plugin_dir.mkdir(parents=True)
        (plugin_dir / "plugin.json").write_text(
            json.dumps(
                {
                    "schema_version": 2,
                    "id": plugin_id,
                    "version": "1.0.0",
                    "entrypoints": {"run": "main:run"},
                    "permissions": {
                        "network": [],
                        "filesystem": ["effects"] if effects else [],
                        "secrets": [],
                    },
                    "timeout_seconds": 30,
                    "side_effects": effects,
                    "idempotency_key_required": effects,
                }
            )
        )
        (plugin_dir / "main.py").write_text(source)

    @classmethod
    def tearDownClass(cls):
        for name, value in cls._saved.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        cls._root.cleanup()

    @classmethod
    def cli(cls, arguments, cwd=None):
        completed = subprocess.run(
            [cls.binary, *arguments],
            cwd=cwd or getattr(cls, "project", None),
            capture_output=True,
            text=True,
            check=False,
        )
        return completed.returncode, json.loads(completed.stdout)

    @classmethod
    def cli_json(cls, arguments, cwd=None):
        return cls.cli([*arguments, "--json"], cwd)[1]

    def test_plan_is_identical(self):
        cli = self.cli_json(["plan", "ok.py", "--project-dir", str(self.project)])
        self.assertEqual(self.client.plan(["ok.py"]), cli)

    def test_successful_run_has_the_same_status_nodes_and_mode(self):
        code, cli = self.cli(["run", "ok.py", "--json"])
        self.assertEqual(code, 0)
        sdk = self.client.run(["ok.py"])
        self.assertEqual(sdk.status, cli["status"])
        self.assertEqual(list(sdk.nodes), cli["nodes"])
        self.assertEqual(sdk.execution_mode, cli["execution_mode"])
        self.assertEqual(list(sdk.warnings), cli["warnings"])
        self.assertTrue(sdk.run_id and sdk.session_id)

    def test_failed_run_has_the_same_stable_code_and_no_free_text(self):
        code, cli = self.cli(["run", "fail.py", "--json"])
        self.assertEqual(code, 3)
        sdk = self.client.run(["fail.py"])
        self.assertTrue(sdk.failed)
        self.assertEqual(sdk.error_code, cli["error_code"])
        self.assertEqual(sdk.error_code, "DOWAI_PYTHON_FAILED")
        self.assertNotIn("private detail", sdk.message or "")
        self.assertNotIn("private detail", cli["message"])
        # ENT-015: a failed run that started reports its run_id on both paths. Each path runs
        # its own run, so the identifiers differ; both must name a FAILED run in the journal.
        self.assertTrue(cli["run_id"] and sdk.run_id)
        self.assertEqual(self.cli_json(["runs", "show", cli["run_id"]])["run"]["status"], "FAILED")
        self.assertEqual(self.cli_json(["runs", "show", sdk.run_id])["run"]["status"], "FAILED")

    def test_retry_refusal_has_the_same_code_on_both_paths(self):
        code, cli = self.cli(["retry", UNKNOWN_RUN, "--json"])
        self.assertEqual(code, 2)
        sdk = self.client.retry(UNKNOWN_RUN)
        self.assertEqual(sdk.error_code, cli["error_code"])
        self.assertEqual(sdk.error_code, "DOWAI_RUN_NOT_FOUND")
        self.assertEqual(sdk.status, cli["status"])
        # A refusal starts no run, so neither path reports a run_id.
        self.assertNotIn("run_id", cli)
        self.assertIsNone(sdk.run_id)

    def test_retry_of_a_failed_run_matches_the_cli(self):
        self.client.run(["fail.py"])
        failed_run = self.client.runs(1)["runs"][0]
        self.assertEqual(failed_run["status"], "FAILED")
        code, cli = self.cli(["retry", failed_run["run_id"], "--json"])
        self.assertEqual(code, 3)
        sdk = self.client.retry(failed_run["run_id"])
        self.assertEqual(sdk.status, cli["status"])
        self.assertEqual(sdk.error_code, cli["error_code"])

    def test_journal_views_show_the_same_runs_and_details(self):
        cli_runs = self.cli_json(["runs", "list", "--limit", "5"])
        self.assertEqual(self.client.runs(5), cli_runs)
        run_id = cli_runs["runs"][0]["run_id"]
        self.assertEqual(self.client.run_detail(run_id), self.cli_json(["runs", "show", run_id]))
        self.assertEqual(self.client.logs(run_id), self.cli_json(["logs", run_id]))

    def test_plugin_resolution_is_identical_on_every_path(self):
        cli = self.cli_json(["plugins", "resolve", "email.send"])
        self.assertEqual(cli["entrypoint"], "sender:send")
        self.assertEqual(self.client.plugin_resolve("email.send"), cli)
        env = Env(backend=self.client)
        self.assertEqual(env.plugins.resolve("email.send"), cli)
        self.assertFalse(self.plugin_sentinel.exists(), "resolution must not import the plugin")

    def test_plugin_execution_result_is_identical_on_every_path(self):
        cli = self.cli_json(["plugins", "run", "echo.run", "--input", '{"a": 1}'])
        self.assertEqual(cli["status"], "SUCCEEDED")
        self.assertEqual(cli["result"], {"echo": {"a": 1}})
        self.assertEqual(self.client.plugin_run("echo.run", {"a": 1}), cli)
        env = Env(backend=self.client)
        self.assertEqual(env.plugins.run("echo.run", a=1), cli)
        self.assertEqual(env.plugins.echo.run(a=1), cli)

    def test_plugin_idempotency_is_identical_on_every_path(self):
        args = ["plugins", "run", "notify.run", "--idempotency-key", "sdk-parity"]
        first = self.cli_json(args)
        self.assertEqual(first["status"], "SUCCEEDED")
        # The same key through the client and the facade is a duplicate: same envelope as the CLI.
        duplicate = self.cli_json(args)
        self.assertEqual(duplicate["status"], "DUPLICATE")
        self.assertEqual(
            self.client.plugin_run("notify.run", idempotency_key="sdk-parity"), duplicate
        )
        env = Env(backend=self.client)
        self.assertEqual(env.plugins.notify.run(idempotency_key="sdk-parity"), duplicate)
        self.assertEqual(self.effects.read_text(), "x", "the effect ran exactly once")

    def test_plugin_key_required_has_the_same_code_as_the_cli(self):
        code, cli = self.cli(["plugins", "run", "notify.run", "--json"])
        self.assertEqual(code, 2)
        with self.assertRaises(DowaiExecutionError) as context:
            Env(backend=self.client).plugins.notify.run()
        self.assertEqual(context.exception.code, cli["error_code"])
        self.assertEqual(context.exception.code, "DOWAI_PLUGIN_IDEMPOTENCY_KEY_REQUIRED")

    def test_undeclared_plugin_method_raises_the_same_code_as_the_cli(self):
        code, cli = self.cli(["plugins", "resolve", "email.delete", "--json"])
        self.assertNotEqual(code, 0)
        with self.assertRaises(DowaiExecutionError) as context:
            Env(backend=self.client).plugins.run("email.delete")
        self.assertEqual(context.exception.code, cli["error_code"])
        self.assertEqual(context.exception.code, "DOWAI_CONFIG_INVALID")

    def test_invalid_identifier_raises_the_same_code_as_the_cli(self):
        code, cli = self.cli(["logs", "not-a-uuid", "--json"])
        self.assertEqual(code, 2)
        with self.assertRaises(DowaiExecutionError) as context:
            self.client.logs("not-a-uuid")
        self.assertEqual(context.exception.code, cli["error_code"])


if __name__ == "__main__":
    unittest.main()
