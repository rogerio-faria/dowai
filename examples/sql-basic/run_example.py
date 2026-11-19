"""Executa o exemplo sql-basic (ENT-083) contra PostgreSQL real.

Uso (a partir da raiz do repositório), com as variáveis de conexão exportadas:

    export DOWAI_EXAMPLE_PG_USER=... DOWAI_EXAMPLE_PG_DATABASE=...
    # opcionais: DOWAI_EXAMPLE_PG_HOST (127.0.0.1), DOWAI_EXAMPLE_PG_PORT (5432),
    #            DOWAI_EXAMPLE_PG_TLS (required|disabled; disabled só em loopback),
    #            DOWAI_EXAMPLE_PG_PASSWORD (nome da variável lida pelo dowai; vazia se ausente)
    python3 -I examples/sql-basic/run_example.py

O exemplo é copiado para um diretório temporário, junto de um `profiles.json` gerado a partir
das variáveis acima; nenhuma credencial é gravada no repositório. As tabelas de origem são criadas
no schema `dowai_example_sql_basic_landing` e os modelos `stg_clientes` e `faturamento` são criados
no schema padrão do usuário do perfil. Tudo que o exemplo cria é removido ao fim.

Requer `psql` e o binário `dowai` em `target/debug/dowai` ou em `DOWAI_BIN`.
Sai com 0 somente se as duas execuções terminarem com SUCCEEDED e os resultados forem os esperados.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SOURCE_SCHEMA = "dowai_example_sql_basic_landing"
COPY_IGNORE = shutil.ignore_patterns("__pycache__", "run_example.py", "README.md")
EXPECTED_ROWS = ["1|Ana|150.50", "2|Bruno|200.00"]


def dowai_binary():
    return os.environ.get("DOWAI_BIN") or os.path.join(ROOT, "target", "debug", "dowai")


def connection_settings():
    user = os.environ.get("DOWAI_EXAMPLE_PG_USER")
    database = os.environ.get("DOWAI_EXAMPLE_PG_DATABASE")
    if not user or not database:
        print(
            "Defina DOWAI_EXAMPLE_PG_USER e DOWAI_EXAMPLE_PG_DATABASE (ver README).",
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


def psql_env(settings):
    env = dict(os.environ)
    env["PGPASSWORD"] = env.get("DOWAI_EXAMPLE_PG_PASSWORD", "")
    env["PGSSLMODE"] = "disable" if settings["tls"] == "disabled" else "require"
    return env


def psql(settings, sql, capture=False):
    completed = subprocess.run(
        [
            "psql", "-X", "-q", "-A", "-t", "-F", "|",
            "-v", "ON_ERROR_STOP=1",
            "-h", settings["host"], "-p", str(settings["port"]),
            "-U", settings["user"], "-d", settings["database"],
            "-c", sql,
        ],
        env=psql_env(settings),
        capture_output=True,
        text=True,
        timeout=120,
    )
    if completed.returncode != 0:
        raise RuntimeError(f"psql falhou: {completed.stderr.strip()}")
    return completed.stdout.strip() if capture else None


def reset(settings):
    psql(
        settings,
        f"DROP TABLE IF EXISTS faturamento, stg_clientes; "
        f"DROP SCHEMA IF EXISTS {SOURCE_SCHEMA} CASCADE;",
    )


def seed(settings):
    psql(
        settings,
        f"""
        CREATE SCHEMA {SOURCE_SCHEMA};
        CREATE TABLE {SOURCE_SCHEMA}.clientes (id integer PRIMARY KEY, nome text NOT NULL);
        CREATE TABLE {SOURCE_SCHEMA}.vendas (
            id integer PRIMARY KEY, cliente_id integer NOT NULL, valor numeric(12,2) NOT NULL
        );
        INSERT INTO {SOURCE_SCHEMA}.clientes VALUES (1,'Ana'), (2,'Bruno'), (3,'Carla');
        INSERT INTO {SOURCE_SCHEMA}.vendas VALUES
            (1, 1, 100.00), (2, 1, 50.50), (3, 2, 200.00);
        """,
    )


def run_dowai(project, work, extra_args):
    env = dict(os.environ)
    for name in ("XDG_STATE_HOME", "XDG_CONFIG_HOME"):
        env[name] = os.path.join(work, name.lower())
    completed = subprocess.run(
        [dowai_binary(), "run", "faturamento.sql", *extra_args, "--json"],
        cwd=project,
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
    )
    report = None
    try:
        report = json.loads(completed.stdout)
    except json.JSONDecodeError:
        pass
    return completed.returncode, report, completed


def main():
    settings = connection_settings()
    if settings is None:
        return 2
    # O dowai exige que a variável indicada em password exista (vazia em autenticação confiável).
    os.environ.setdefault("DOWAI_EXAMPLE_PG_PASSWORD", "")
    work = tempfile.mkdtemp(prefix="dowai-example-sql-basic-")
    try:
        project = os.path.join(work, "project")
        shutil.copytree(HERE, project, ignore=COPY_IGNORE)
        write_workspace(work, settings)
        reset(settings)
        try:
            seed(settings)
            checks = {}

            completed = None
            for label, extra_args in (
                ("primeira execução", []),
                ("segunda execução (--yes: substitui as relações)", ["--yes"]),
            ):
                code, report, completed = run_dowai(project, work, extra_args)
                succeeded = code == 0 and bool(report) and report.get("status") == "SUCCEEDED"
                checks[f"{label} SUCCEEDED"] = succeeded
                if not succeeded:
                    print(f"FALHA {label}:", file=sys.stderr)
                    print(completed.stdout, completed.stderr, file=sys.stderr)
                    return 1
                rows = psql(
                    settings,
                    "SELECT cliente_id, nome, total FROM faturamento ORDER BY cliente_id",
                    capture=True,
                ).split("\n")
                checks[f"{label}: resultado esperado"] = rows == EXPECTED_ROWS

            for label, ok in checks.items():
                print(("OK    " if ok else "FALHA ") + label)
            if not all(checks.values()):
                return 1
            print("sql-basic: OK")
            return 0
        finally:
            reset(settings)
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"FALHA: {error}", file=sys.stderr)
        return 1
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
