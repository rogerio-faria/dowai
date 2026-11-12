#!/usr/bin/env bash
# ENT-055: valida que o Pyright aceita os stubs gerados por `dowai plugins sync` e rejeita uso
# incorreto. Não executa código de plugin: apenas gera stubs e roda o type checker.
# Uso: validate.sh <binario-dowai> <binario-pyright>
set -euo pipefail

DOWAI="$1"
PYRIGHT="$2"
FIXTURE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

export XDG_STATE_HOME="$WORK/state" XDG_CONFIG_HOME="$WORK/config"
export LOCALAPPDATA="$WORK/local" APPDATA="$WORK/appdata" PROGRAMDATA="$WORK/programdata"
mkdir -p "$WORK/workspace"
"$DOWAI" init demo --workspace "$WORK/workspace" --json > "$WORK/init.json"
PROJECT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["project_dir"])' "$WORK/init.json")"

cp -r "$FIXTURE/plugins" "$PROJECT/"
cp "$FIXTURE/pyrightconfig.json" "$FIXTURE/app_ok.py" "$FIXTURE/app_bad.py" "$PROJECT/"
cd "$PROJECT"

"$DOWAI" plugins sync
echo "--- pyright: uso correto (deve passar)"
"$PYRIGHT" app_ok.py
echo "--- pyright: uso incorreto (deve falhar)"
if "$PYRIGHT" app_bad.py > "$WORK/bad.txt" 2>&1; then
  echo "FALHA: pyright aceitou uso incorreto" >&2
  exit 1
fi
cat "$WORK/bad.txt"
grep -q 'list\[str\]' "$WORK/bad.txt"
echo "OK: stubs aceitos pelo Pyright e uso incorreto rejeitado"
