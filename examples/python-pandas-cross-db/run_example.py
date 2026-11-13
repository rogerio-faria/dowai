"""Executa o exemplo python-pandas-cross-db (ENT-084, plano §17.4) contra PostgreSQL real.

Uso (a partir da raiz do repositório), com as variáveis de conexão exportadas:

    export DOWAI_EXAMPLE_PG_USER=... DOWAI_EXAMPLE_PG_DATABASE=...   # origem (perfil warehouse)
    export DOWAI_EXAMPLE_FIN_DATABASE=...                            # destino (perfil financeiro)
    # opcionais: DOWAI_EXAMPLE_PG_HOST (127.0.0.1), DOWAI_EXAMPLE_PG_PORT (5432),
    #            DOWAI_EXAMPLE_PG_TLS (required|disabled; disabled só em loopback),
    #            DOWAI_EXAMPLE_PG_PASSWORD (vazia se a autenticação for confiável)
    python3 -I examples/python-pandas-cross-db/run_example.py

O exemplo é copiado para um diretório temporário. Um venv `.venv` é provisionado a partir de
`requirements.lock` com `uv pip sync --require-hashes` (o runtime não instala pacotes). O
`workspace.json` é gerado com os perfis `warehouse` (origem) e `financeiro` (destino).

O modelo `comissao.py` lê `faturamento` com `env.ref(...).to_pandas()`, calcula a comissão de 5% e
grava com `env.write(df, target="financeiro", table="comissoes", mode="append")`. O runner cria a
tabela de destino, executa, confere as linhas gravadas e remove as tabelas que criou.

Requer `psql`, `uv` (ou `UV_BIN`) e o binário `dowai` em `target/debug/dowai` (ou `DOWAI_BIN`).
Sai com 0 somente se a execução terminar SUCCEEDED e o destino contiver as linhas esperadas.
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
SOURCE_TABLE = "faturamento"
DESTINATION_TABLE = "comissoes"
EXPECTED_COMMISSIONS = ["1|1500|75", "2|800|40", "3|120|6"]

failures = []


def check(ok, label):
    print(("OK    " if ok else "FALHA ") + label)
    if not ok:
        failures.append(label)


def dowai_binary():
    return os.environ.get("DOWAI_BIN") or os.path.join(ROOT, "target", "debug", "dowai")


def uv_binary():
    return os.environ.get("UV_BIN") or shutil.which("uv")


def connection_settings():
    user = os.environ.get("DOWAI_EXAMPLE_PG_USER")
    database = os.environ.get("DOWAI_EXAMPLE_PG_DATABASE")
    financial = os.environ.get("DOWAI_EXAMPLE_FIN_DATABASE")
    if not user or not database or not financial:
        print(
            "Defina DOWAI_EXAMPLE_PG_USER, DOWAI_EXAMPLE_PG_DATABASE e DOWAI_EXAMPLE_FIN_DATABASE.",
            file=sys.stderr,
        )
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
        "financial": financial,
        "tls": tls,
    }


def write_workspace(work, settings):
    def profile(database):
        return {
            "type": "postgres",
            "host": settings["host"],
            "port": settings["port"],
            "user": settings["user"],
            "database": database,
            "password_env": "DOWAI_EXAMPLE_PG_PASSWORD",
            "tls": settings["tls"],
        }

    workspace = {
        "schema_version": 2,
        "profiles": {
            "warehouse": profile(settings["database"]),
            "financeiro": profile(settings["financial"]),
        },
        "defaults": {"execution": {"default_profile": "warehouse"}},
    }
    with open(os.path.join(work, "workspace.json"), "w", encoding="utf-8") as handle:
        json.dump(workspace, handle)


def provision_venv(uv, project):
    """Venv com as dependências do lockfile, com hashes. O runtime não instala pacotes."""
    venv = os.path.join(project, ".venv")
    subprocess.run([uv, "venv", "--python", sys.executable, "--quiet", venv], check=True)
    subprocess.run(
        [uv, "pip", "sync", "--require-hashes", "--python", os.path.join(venv, "bin", "python"),
         os.path.join(project, "requirements.lock"), "--quiet"],
        check=True,
        env={**os.environ, "UV_PYTHON_DOWNLOADS": "never"},
    )
    manifest_path = os.path.join(project, "dowai_project.json")
    with open(manifest_path, encoding="utf-8") as handle:
        manifest = json.load(handle)
    manifest["python"]["interpreter"] = os.path.join(venv, "bin", "python")
    with open(manifest_path, "w", encoding="utf-8") as handle:
        json.dump(manifest, handle)


def psql(settings, database, sql):
    env = dict(os.environ)
    if settings["tls"] == "disabled":
        env["PGSSLMODE"] = "disable"
    if os.environ.get("DOWAI_EXAMPLE_PG_PASSWORD"):
        env["PGPASSWORD"] = os.environ["DOWAI_EXAMPLE_PG_PASSWORD"]
    return subprocess.run(
        ["psql", "-h", settings["host"], "-p", str(settings["port"]), "-U", settings["user"],
         "-d", database, "-X", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-c", sql],
        env=env,
        capture_output=True,
        text=True,
    )


def run_dowai(binary, project, target):
    result = subprocess.run(
        [binary, "run", target, "--yes", "--json"],
        cwd=project,
        capture_output=True,
        text=True,
    )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
        payload = {}
    return result.returncode, payload


def runs_show(binary, project, run_id):
    result = subprocess.run(
        [binary, "runs", "show", run_id, "--json"], cwd=project, capture_output=True, text=True
    )
    return json.loads(result.stdout)["run"]


def main():
    settings = connection_settings()
    if settings is None:
        return 2
    binary = dowai_binary()
    if not os.path.isfile(binary):
        print(f"Binário dowai não encontrado: {binary} (compile com cargo build).", file=sys.stderr)
        return 2
    uv = uv_binary()
    if not uv:
        print("uv não encontrado (defina UV_BIN ou instale a versão fixada em ENT-004).", file=sys.stderr)
        return 2
    if not shutil.which("psql"):
        print("psql não encontrado no PATH.", file=sys.stderr)
        return 2

    work = tempfile.mkdtemp(prefix="dowai-pandas-cross-db-")
    try:
        project = os.path.join(work, "project")
        shutil.copytree(HERE, project, ignore=COPY_IGNORE)
        write_workspace(work, settings)
        provision_venv(uv, project)

        psql(settings, settings["database"], f"DROP TABLE IF EXISTS {SOURCE_TABLE}")
        psql(settings, settings["financial"], f"DROP TABLE IF EXISTS {DESTINATION_TABLE}")
        created = psql(
            settings,
            settings["financial"],
            f"CREATE TABLE {DESTINATION_TABLE} (cliente_id integer, total numeric, comissao numeric)",
        )
        check(created.returncode == 0, "tabela de destino criada no perfil financeiro")

        code, payload = run_dowai(binary, project, "comissao.py")
        check(code == 0 and payload.get("status") == "SUCCEEDED", "comissao.py: SUCCEEDED")
        if payload.get("run_id"):
            invocations = runs_show(binary, project, payload["run_id"])["invocations"]
            source = [i for i in invocations if i["target"] == "faturamento.sql"]
            check(
                bool(source) and source[0]["status"] == "SUCCEEDED",
                "faturamento.sql (predecessor por env.ref) executou antes do modelo",
            )

        result = psql(
            settings,
            settings["financial"],
            f"SELECT cliente_id, total::float8, comissao::float8 FROM {DESTINATION_TABLE} ORDER BY 1",
        )
        rows = [line.strip() for line in result.stdout.splitlines() if line.strip()]
        check(rows == EXPECTED_COMMISSIONS, f"destino com as comissões esperadas: {rows}")
    finally:
        psql(settings, settings["database"], f"DROP TABLE IF EXISTS {SOURCE_TABLE}")
        psql(settings, settings["financial"], f"DROP TABLE IF EXISTS {DESTINATION_TABLE}")
        shutil.rmtree(work, ignore_errors=True)

    if failures:
        print(f"python-pandas-cross-db: {len(failures)} expectativa(s) não atendida(s)")
        return 1
    print("python-pandas-cross-db: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
