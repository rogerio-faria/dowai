"""Executa os exemplos de plugin (ENT-085) contra API falsa e plugin local.

Executar a partir da raiz do repositório:
    python3 -I -m unittest discover -s tests/examples -p "test_*.py"

Requer `target/debug/dowai` (ou `DOWAI_BIN`), construído com `cargo build -p dowai-cli --locked`.
"""

import os
import subprocess
import sys
import unittest

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
EXAMPLES = ("rest-pagination", "local-plugin-override")


class PluginExamplesTest(unittest.TestCase):
    def test_examples_run_against_fake_api_with_local_plugin(self):
        for name in EXAMPLES:
            with self.subTest(example=name):
                script = os.path.join(ROOT, "examples", name, "run_example.py")
                completed = subprocess.run(
                    [sys.executable, "-I", script],
                    cwd=ROOT,
                    capture_output=True,
                    text=True,
                    timeout=300,
                )
                self.assertEqual(
                    completed.returncode,
                    0,
                    f"{name} falhou:\n{completed.stdout}\n{completed.stderr}",
                )


if __name__ == "__main__":
    unittest.main()
