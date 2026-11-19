"""Executa o exemplo manual-sequence (ENT-084, plano §17.3) contra PostgreSQL real.

Uso (a partir da raiz do repositório), com as variáveis de conexão exportadas:

    export DOWAI_EXAMPLE_PG_USER=... DOWAI_EXAMPLE_PG_DATABASE=...
    # opcionais: DOWAI_EXAMPLE_PG_HOST (127.0.0.1), DOWAI_EXAMPLE_PG_PORT (5432),
    #            DOWAI_EXAMPLE_PG_TLS (required|disabled; disabled só em loopback),
    #            DOWAI_EXAMPLE_PG_PASSWORD (vazia se a autenticação for confiável)
    python3 -I examples/manual-sequence/run_example.py

O exemplo é copiado para um diretório temporário, com um venv Python sem pacotes e um
`profiles.json` gerado a partir das variáveis acima; nenhuma credencial é gravada no repositório.
Executa duas sequências:

- `sequencia.py`: com `try` em torno de `arq10.sql`. Deve terminar SUCCEEDED, com `arq7.py`
  executado e as três variáveis publicadas.
- `sequencia_sem_tratamento.py`: sem `try`. Deve terminar FAILED com `DOWAI_EXECUTION_FAILED`,
  sem nenhuma invocação de `arq7.py` no journal.

Ao fim remove a tabela `arq1` criada pelo exemplo e o diretório temporário.
Requer `psql`, `python3` com `venv` e o binário `dowai` em `target/debug/dowai` ou em `DOWAI_BIN`.
Sai com 0 somente se todas as expectativas forem atendidas.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
COPY_IGNORE = shutil.ignore_patterns("__pycache__", "run_example.py", "README.md")
CREATED_TABLES = ["arq1"]
EXPECTED_VARIABLES = {"carga_total", "falha_tolerada", "arq7_executado"}

failures = []


def check(ok, label):
    print(("OK    " if ok else "FALHA ") + label)
    if not ok:
        failures.append(label)


def dowai_binary():
    return os.environ.get("DOWAI_BIN") or os.path.join(ROOT, "target", "debug", "dowai")


def connection_settings():
    user = os.environ.get("DOWAI_EXAMPLE_PG_USER")
    database = os.environ.get("DOWAI_EXAMPLE_PG_DATABASE")
    if not user or not database:
        print("Defina DOWAI_EXAMPLE_PG_USER e DOWAI_EXAMPLE_PG_DATABASE (ver README).", file=sys.stderr)
        return None
    tls = os.environ.get("DOWAI_EXAMPLE_PG_TLS", "required")
    if tls not in ("required", "disabled"):
        print("DOWAI_EXAMPLE_PG_TLS deve ser 'required' ou 'disabled'.", file=sys.stderr)
        return None
    return {
        "host": os.environ.get("DOWAI_EXAMPLE_PG_HOST", "127.0.0.1"),
        "port": int(os.environ.get("DOWAI_EXAMPLE_PG_PORT", "5432")),
        "user": user,
        "database": database,
        "tls": tls,
    }


def write_workspace(work, settings):
    workspace = {
        "schema_version": 2,
        "profiles": {
            "warehouse": {
                "type": "postgres",
                "host": settings["host"],
                "port": settings["port"],
                "user": settings["user"],
                "database": settings["database"],
                "password": "{{ env_var('DOWAI_EXAMPLE_PG_PASSWORD') }}",
                "tls": settings["tls"],
            }
        },
        "defaults": {"execution": {"default_profile": "warehouse"}},
    }
    with open(os.path.join(work, "profiles.json"), "w", encoding="utf-8") as handle:
        json.dump(workspace, handle)


def prepare_venv(work):
    """Venv sem pacotes: os modelos deste exemplo usam apenas a biblioteca padrão."""
    venv = os.path.join(work, ".venv")
    subprocess.run([sys.executable, "-m", "venv", "--without-pip", venv], check=True)
    manifest_path = os.path.join(work, "dowai_project.json")
    with open(manifest_path, encoding="utf-8") as handle:
        manifest = json.load(handle)
    manifest["python"]["interpreter"] = os.path.join(venv, "bin", "python")
    with open(manifest_path, "w", encoding="utf-8") as handle:
        json.dump(manifest, handle)


def run_dowai(binary, work, target, extra=()):
    result = subprocess.run(
        [binary, "run", target, "--yes", "--json", *extra],
        cwd=work,
        capture_output=True,
        text=True,
    )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
        payload = {}
    return result.returncode, payload


def runs_show(binary, work, run_id):
    result = subprocess.run(
        [binary, "runs", "show", run_id, "--json"],
        cwd=work,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)["run"]


def session_variable_names(binary, work, session_id):
    result = subprocess.run(
        [binary, "session", "show", session_id, "--json"],
        cwd=work,
        capture_output=True,
        text=True,
    )
    return set(json.loads(result.stdout)["variable_names"])


def drop_tables(settings):
    psql = shutil.which("psql")
    if not psql:
        return
    env = dict(os.environ)
    if settings["tls"] == "disabled":
        env["PGSSLMODE"] = "disable"
    if os.environ.get("DOWAI_EXAMPLE_PG_PASSWORD"):
        env["PGPASSWORD"] = os.environ["DOWAI_EXAMPLE_PG_PASSWORD"]
    for table in CREATED_TABLES:
        subprocess.run(
            [
                psql, "-h", settings["host"], "-p", str(settings["port"]),
                "-U", settings["user"], "-d", settings["database"],
                "-qc", f"DROP TABLE IF EXISTS {table}",
            ],
            env=env,
            capture_output=True,
        )


def main():
    settings = connection_settings()
    if settings is None:
        return 2
    binary = dowai_binary()
    if not os.path.isfile(binary):
        print(f"Binário dowai não encontrado: {binary} (compile com cargo build).", file=sys.stderr)
        return 2

    work = tempfile.mkdtemp(prefix="dowai-manual-sequence-")
    try:
        project = os.path.join(work, "project")
        shutil.copytree(HERE, project, ignore=COPY_IGNORE)
        write_workspace(work, settings)
        prepare_venv(project)

        code, payload = run_dowai(binary, project, "sequencia.py")
        check(code == 0 and payload.get("status") == "SUCCEEDED", "sequencia.py com try: SUCCEEDED")
        if payload.get("session_id"):
            names = session_variable_names(binary, project, payload["session_id"])
            check(names == EXPECTED_VARIABLES, "sequencia.py: variáveis publicadas " + str(sorted(names)))
        if payload.get("run_id"):
            invocations = runs_show(binary, project, payload["run_id"])["invocations"]
            arq10 = [i for i in invocations if i["target"] == "arq10.sql"]
            arq7 = [i for i in invocations if i["target"] == "arq7.py"]
            check(
                bool(arq10) and arq10[0]["status"] == "FAILED" and arq10[0]["error_code"] == "DOWAI_EXECUTION_FAILED",
                "sequencia.py: arq10.sql falhou e a falha foi tolerada",
            )
            check(bool(arq7) and arq7[0]["status"] == "SUCCEEDED", "sequencia.py: arq7.py executou depois da falha tolerada")

        code, payload = run_dowai(binary, project, "sequencia_sem_tratamento.py")
        check(
            code == 3 and payload.get("status") == "FAILED" and payload.get("error_code") == "DOWAI_EXECUTION_FAILED",
            "sequencia_sem_tratamento.py: FAILED com DOWAI_EXECUTION_FAILED (exit 3)",
        )
        if payload.get("run_id"):
            invocations = runs_show(binary, project, payload["run_id"])["invocations"]
            targets = [i["target"] for i in invocations]
            check("arq7.py" not in targets, "sem try: arq7.py não foi iniciado")
    finally:
        drop_tables(settings)
        shutil.rmtree(work, ignore_errors=True)

    if failures:
        print(f"manual-sequence: {len(failures)} expectativa(s) não atendida(s)")
        return 1
    print("manual-sequence: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
