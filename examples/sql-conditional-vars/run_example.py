"""Executa o exemplo sql-conditional-vars (ENT-083) contra PostgreSQL real.

Uso (a partir da raiz do repositório), com as variáveis de conexão exportadas:

    export DOWAI_EXAMPLE_PG_USER=... DOWAI_EXAMPLE_PG_DATABASE=...
    # opcionais: DOWAI_EXAMPLE_PG_HOST (127.0.0.1), DOWAI_EXAMPLE_PG_PORT (5432),
    #            DOWAI_EXAMPLE_PG_TLS (required|disabled; disabled só em loopback),
    #            DOWAI_EXAMPLE_PG_PASSWORD (nome da variável lida pelo dowai; vazia se ausente)
    python3 -I examples/sql-conditional-vars/run_example.py

Três execuções do mesmo modelo `faturamento.sql`:
  1. sem variáveis: `def_var` usa o padrão `false` e o ramo filtrado é executado;
  2. com `--vars '{"carga_total": true}'`: o valor da CLI prevalece sobre `def_var` e o ramo completo é executado;
  3. sem variáveis de novo: sem `--session` nada é herdado, então volta ao ramo filtrado.

O exemplo é copiado para um diretório temporário com um `profiles.json` gerado a partir das
variáveis acima; nenhuma credencial é gravada no repositório. Tudo que o exemplo cria é removido ao fim.
Requer `psql` e o binário `dowai` em `target/debug/dowai` ou em `DOWAI_BIN`.
Sai com 0 somente se as três execuções terminarem com SUCCEEDED e os resultados forem os esperados.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SOURCE_SCHEMA = "dowai_example_sql_conditional_landing"
COPY_IGNORE = shutil.ignore_patterns("__pycache__", "run_example.py", "README.md")
FILTERED_ROWS = ["1|100.00", "2|200.00"]  # ramo else: somente vendas com valor >= 100
FULL_ROWS = ["1|150.50", "2|200.00"]  # ramo then: todas as vendas
QUERY = "SELECT cliente_id, total FROM faturamento ORDER BY cliente_id"


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


def psql(settings, sql, capture=False):
    env = dict(os.environ)
    env["PGPASSWORD"] = env.get("DOWAI_EXAMPLE_PG_PASSWORD", "")
    env["PGSSLMODE"] = "disable" if settings["tls"] == "disabled" else "require"
    completed = subprocess.run(
        [
            "psql", "-X", "-q", "-A", "-t", "-F", "|",
            "-v", "ON_ERROR_STOP=1",
            "-h", settings["host"], "-p", str(settings["port"]),
            "-U", settings["user"], "-d", settings["database"],
            "-c", sql,
        ],
        env=env,
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
        f"DROP TABLE IF EXISTS faturamento; DROP SCHEMA IF EXISTS {SOURCE_SCHEMA} CASCADE;",
    )


def seed(settings):
    psql(
        settings,
        f"""
        CREATE SCHEMA {SOURCE_SCHEMA};
        CREATE TABLE {SOURCE_SCHEMA}.vendas (
            id integer PRIMARY KEY, cliente_id integer NOT NULL, valor numeric(12,2) NOT NULL
        );
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
    succeeded = completed.returncode == 0 and bool(report) and report.get("status") == "SUCCEEDED"
    return succeeded, completed


def main():
    settings = connection_settings()
    if settings is None:
        return 2
    # O dowai exige que a variável indicada em password exista (vazia em autenticação confiável).
    os.environ.setdefault("DOWAI_EXAMPLE_PG_PASSWORD", "")
    work = tempfile.mkdtemp(prefix="dowai-example-sql-vars-")
    try:
        project = os.path.join(work, "project")
        shutil.copytree(HERE, project, ignore=COPY_IGNORE)
        write_workspace(work, settings)
        reset(settings)
        try:
            seed(settings)
            scenarios = (
                ("sem variáveis (padrão false)", [], FILTERED_ROWS),
                ('--vars {"carga_total": true}', ["--yes", "--vars", '{"carga_total": true}'], FULL_ROWS),
                ("sem variáveis após execução com variável (sem --session)", ["--yes"], FILTERED_ROWS),
            )
            checks = {}
            for label, extra_args, expected in scenarios:
                succeeded, completed = run_dowai(project, work, extra_args)
                checks[f"{label}: SUCCEEDED"] = succeeded
                if not succeeded:
                    print(f"FALHA {label}:", file=sys.stderr)
                    print(completed.stdout, completed.stderr, file=sys.stderr)
                    return 1
                rows = psql(settings, QUERY, capture=True).split("\n")
                checks[f"{label}: resultado esperado"] = rows == expected

            for label, ok in checks.items():
                print(("OK    " if ok else "FALHA ") + label)
            if not all(checks.values()):
                return 1
            print("sql-conditional-vars: OK")
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
