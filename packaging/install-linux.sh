#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "Run this installer as root (for example, with sudo)." >&2
  exit 1
fi

dependencies=$(ldd ./dowai)
printf '%s\n' "$dependencies"
if printf '%s\n' "$dependencies" | grep -q 'not found'; then
  echo "Missing shared libraries; install them before running DOWAI." >&2
  exit 1
fi
install -d /usr/local/bin
install -m 755 ./dowai /usr/local/bin/dowai
echo "Installed /usr/local/bin/dowai"
