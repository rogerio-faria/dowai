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

from dowai import DowaiClient, DowaiExecutionError  # noqa: E402

REPOSITORY_ROOT = SDK_ROOT.parents[1]
UNKNOWN_RUN = "00000000-0000-4000-8000-000000000000"


def _engine_binary():
    candidate = os.environ.get("DOWAI_BIN") or str(REPOSITORY_ROOT / "target" / "debug" / "dowai")
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
        manifest_path.write_text(json.dumps(manifest))
        (cls.project / "models" / "ok.py").write_text(
            "def model(env):\n    env.setVar('x', 1)\n"
        )
        (cls.project / "models" / "fail.py").write_text(
            "def model(env):\n    raise RuntimeError('private detail')\n"
        )
        cls.client = DowaiClient(cls.project, binary=cls.binary)

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

    def test_invalid_identifier_raises_the_same_code_as_the_cli(self):
        code, cli = self.cli(["logs", "not-a-uuid", "--json"])
        self.assertEqual(code, 2)
        with self.assertRaises(DowaiExecutionError) as context:
            self.client.logs("not-a-uuid")
        self.assertEqual(context.exception.code, cli["error_code"])


if __name__ == "__main__":
    unittest.main()
