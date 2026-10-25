#!/usr/bin/env bash
# Actualiza el bundle de Vencord (build "userscript") que se inyecta en el webview.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/src-tauri/resources/vencord.js"
URL="${VENCORD_URL:-https://github.com/Vendicated/Vencord/releases/download/devbuild/Vencord.user.js}"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

curl -fsSL "$URL" -o "$tmp"
grep -q '@name            Vencord' "$tmp" || {
  echo "El archivo descargado no parece un build de Vencord" >&2
  exit 1
}

mv "$tmp" "$OUT"
chmod 0644 "$OUT"
echo "Vencord actualizado: $(grep -m1 '@version' "$OUT" | awk '{print $3}')"
