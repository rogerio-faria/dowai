"""Executa o exemplo local-plugin-override contra uma API HTTP falsa (ENT-085, plano §17.5).

Uso (a partir da raiz do repositório):
    python3 -I examples/local-plugin-override/run_example.py

Requer o binário `dowai` em `target/debug/dowai` ou em `DOWAI_BIN`. O exemplo é copiado para um
diretório temporário, onde é criado o venv do projeto; o repositório não recebe `.venv` nem estado.
Verifica: (1) o plugin local substitui o global com origem registrada; (2) a execução usa o plugin
local e chama a API falsa; (3) com `plugins.allow_local_override = false` a substituição é recusada.
Sai com 0 somente se todas as verificações passarem.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
COPY_IGNORE = shutil.ignore_patterns("__pycache__", "run_example.py", ".gitkeep")


def dowai_binary():
    return os.environ.get("DOWAI_BIN") or os.path.join(ROOT, "target", "debug", "dowai")


class _FakeApiHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        body = json.dumps({"ok": True, "path": self.path}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def start_fake_api():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeApiHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}/status"


def prepare(work):
    """Copia o exemplo e cria o venv do projeto dentro da cópia."""
    example = os.path.join(work, "example")
    shutil.copytree(HERE, example, ignore=COPY_IGNORE)
    project = os.path.join(example, "project")
    venv = os.path.join(project, ".venv")
    subprocess.run([sys.executable, "-m", "venv", "--without-pip", venv], check=True, timeout=120)
    manifest_path = os.path.join(project, "dowai_project.json")
    with open(manifest_path, encoding="utf-8") as handle:
        manifest = json.load(handle)
    manifest["python"] = {"interpreter": os.path.join(venv, "bin", "python"), "timeout_seconds": 30}
    with open(manifest_path, "w", encoding="utf-8") as handle:
        json.dump(manifest, handle)
    return project, manifest_path


def dowai(work, cwd, args):
    env = dict(os.environ)
    for name in ("XDG_STATE_HOME", "XDG_CONFIG_HOME", "LOCALAPPDATA", "APPDATA", "PROGRAMDATA"):
        env[name] = os.path.join(work, name.lower())
    return subprocess.run(
        [dowai_binary(), *args, "--json"],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )


def parse(completed):
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        return {}


def report(label, ok):
    print(("OK    " if ok else "FALHA ") + label)
    return ok


def main():
    work = tempfile.mkdtemp(prefix="dowai-example-override-")
    server = None
    try:
        project, manifest_path = prepare(work)
        server, url = start_fake_api()
        results = []

        resolved = parse(dowai(work, project, ["plugins", "resolve", "api.call"]))
        results.append(report("plugin local substitui o global (replaced_origin=global)",
                              resolved.get("override", {}).get("replaced_origin") == "global"))

        ran = dowai(work, project, ["plugins", "run", "api.call", "--input", json.dumps({"url": url})])
        payload = parse(ran)
        result = payload.get("result", {}) if isinstance(payload.get("result"), dict) else {}
        results.append(report("execução SUCCEEDED pelo plugin local",
                              ran.returncode == 0 and payload.get("status") == "SUCCEEDED"))
        results.append(report("origem 'local' e API falsa respondeu 200",
                              result.get("origin") == "local" and result.get("status") == 200
                              and (result.get("body") or {}).get("ok") is True))

        with open(manifest_path, encoding="utf-8") as handle:
            manifest = json.load(handle)
        manifest["config"]["plugins"]["allow_local_override"] = False
        with open(manifest_path, "w", encoding="utf-8") as handle:
            json.dump(manifest, handle)
        refused = parse(dowai(work, project, ["plugins", "resolve", "api.call"]))
        results.append(report("override recusado quando proibido (DOWAI_POLICY_DENIED)",
                              refused.get("error_code") == "DOWAI_POLICY_DENIED"))

        if not all(results):
            print(json.dumps(payload, ensure_ascii=False, indent=2), file=sys.stderr)
            return 1
        print("local-plugin-override: OK")
        return 0
    finally:
        if server is not None:
            server.shutdown()
            server.server_close()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
