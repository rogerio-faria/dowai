#!/usr/bin/env bash
# Ambiente Python por projeto com uv (ADR 0003, D04; ENT-046).
#
# Uso:
#   scripts/python-env.sh lock <projeto>   gera requirements.lock (hashes) a partir de python.dependencies
#   scripts/python-env.sh sync <projeto>   cria/atualiza o venv .dowai_venv somente a partir do lockfile
#   scripts/python-env.sh freeze <projeto> lista as versões instaladas no venv
#
# Fonte canônica: dowai_project.json (python.dependencies). Índice: PyPI oficial por padrão;
# DOWAI_PY_INDEX substitui o índice somente por configuração explícita do administrador.
# Nunca baixa interpretador (UV_PYTHON_DOWNLOADS=never). Executado fora do run produtivo.
set -euo pipefail

usage() { sed -n '2,9p' "$0" >&2; exit 2; }

[ "$#" -eq 2 ] || usage
command_name="$1"
project_dir="$(cd "$2" && pwd)"
manifest="$project_dir/dowai_project.json"
[ -f "$manifest" ] || { echo "dowai_project.json não encontrado em $project_dir" >&2; exit 2; }

index_url="${DOWAI_PY_INDEX:-https://pypi.org/simple}"
export UV_PYTHON_DOWNLOADS=never
uv_bin="${UV_BIN:-uv}"
command -v "$uv_bin" >/dev/null || { echo "uv não encontrado (versão fixada em ENT-004)" >&2; exit 3; }

interpreter="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["python"]["interpreter"])' "$manifest")"
venv="$project_dir/.dowai_venv"

case "$command_name" in
  lock)
    python3 -I - "$manifest" "$project_dir/requirements.in" <<'PY'
import json, sys
manifest, target = sys.argv[1], sys.argv[2]
deps = json.load(open(manifest, encoding="utf-8"))["python"].get("dependencies", [])
for dep in deps:
    if not dep.strip() or any(ord(c) < 32 for c in dep):
        raise SystemExit("dependência inválida em python.dependencies")
with open(target, "w", encoding="utf-8") as fh:
    fh.write("# Gerado por scripts/python-env.sh a partir de dowai_project.json. Não editar.\n")
    for dep in deps:
        fh.write(dep.strip() + "\n")
PY
    "$uv_bin" pip compile "$project_dir/requirements.in" \
      --generate-hashes --index-url "$index_url" --python "$interpreter" \
      --output-file "$project_dir/requirements.lock" --quiet
    echo "requirements.lock gerado (índice: $index_url)"
    ;;
  sync)
    [ -f "$project_dir/requirements.lock" ] || { echo "requirements.lock ausente: rode lock antes" >&2; exit 2; }
    "$uv_bin" venv --python "$interpreter" --clear --quiet "$venv"
    "$uv_bin" pip sync --require-hashes --index-url "$index_url" \
      --python "$venv/bin/python" "$project_dir/requirements.lock"
    echo "venv sincronizado a partir de requirements.lock"
    ;;
  freeze)
    [ -d "$venv" ] || { echo ".dowai_venv ausente" >&2; exit 2; }
    "$uv_bin" pip freeze --python "$venv/bin/python"
    ;;
  *)
    usage
    ;;
esac
