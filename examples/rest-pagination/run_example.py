"""Executa o exemplo rest-pagination contra a API falsa (ENT-085).

Uso (a partir da raiz do repositório):
    python3 -I examples/rest-pagination/run_example.py

Requer o binário `dowai` em `target/debug/dowai` ou em `DOWAI_BIN`. O exemplo é copiado para um
diretório temporário, onde é criado o venv do projeto (`python.interpreter` exige venv interno);
o repositório não recebe `.venv` nem estado do CLI. Sai com 0 somente se o resultado for o esperado.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, HERE)

from fake_api import FakeApi  # noqa: E402

EXPECTED_ITEMS = [{"id": i} for i in range(1, 6)]
COPY_IGNORE = shutil.ignore_patterns("__pycache__", "run_example.py", "fake_api.py", ".gitkeep")


def dowai_binary():
    return os.environ.get("DOWAI_BIN") or os.path.join(ROOT, "target", "debug", "dowai")


def prepare_project(work):
    """Copia o exemplo para `work/project` e cria o venv do projeto dentro da cópia."""
    project = os.path.join(work, "project")
    shutil.copytree(HERE, project, ignore=COPY_IGNORE)
    venv = os.path.join(project, ".venv")
    subprocess.run([sys.executable, "-m", "venv", "--without-pip", venv], check=True, timeout=120)
    manifest_path = os.path.join(project, "dowai_project.json")
    with open(manifest_path, encoding="utf-8") as handle:
        manifest = json.load(handle)
    manifest["python"] = {
        "interpreter": os.path.join(venv, "bin", "python"),
        "timeout_seconds": 30,
    }
    with open(manifest_path, "w", encoding="utf-8") as handle:
        json.dump(manifest, handle)
    return project


def main():
    work = tempfile.mkdtemp(prefix="dowai-example-rest-")
    try:
        project = prepare_project(work)
        with FakeApi() as api:
            payload = {
                "url": api.url(),
                "include_items": True,
                "backoff_seconds": 0,
                "max_attempts": 3,
                "timeout_seconds": 5,
            }
            env = dict(os.environ)
            for name in ("XDG_STATE_HOME", "XDG_CONFIG_HOME", "LOCALAPPDATA", "APPDATA", "PROGRAMDATA"):
                env[name] = os.path.join(work, name.lower())
            completed = subprocess.run(
                [
                    dowai_binary(),
                    "plugins",
                    "run",
                    "rest.get_pages",
                    "--input",
                    json.dumps(payload),
                    "--idempotency-key",
                    f"rest-pagination-{uuid.uuid4()}",
                    "--json",
                ],
                cwd=project,
                env=env,
                capture_output=True,
                text=True,
                timeout=120,
            )
            if completed.returncode != 0:
                print("FALHA: dowai plugins run saiu com", completed.returncode, file=sys.stderr)
                print(completed.stdout, completed.stderr, file=sys.stderr)
                return 1
            report = json.loads(completed.stdout)
            result = report.get("result", {})
            checks = {
                "status SUCCEEDED": report.get("status") == "SUCCEEDED",
                "3 páginas": result.get("pages") == 3,
                "5 registros": result.get("records") == 5,
                "1 reexecução por 429": result.get("retries") == 1,
                "itens sem duplicação": result.get("items") == EXPECTED_ITEMS,
                "cursores lidos na ordem": api.requests == [None, "c2", "c2", "c3"],
            }
            for label, ok in checks.items():
                print(("OK    " if ok else "FALHA ") + label)
            if not all(checks.values()):
                print(json.dumps(report, ensure_ascii=False, indent=2), file=sys.stderr)
                return 1
            print("rest-pagination: OK")
            return 0
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
