#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_ZIP="${OUT_ZIP:-}"
DEMO_EPF="$ROOT_DIR/demo/Demo.epf"

log() {
  printf '%s\n' "$*"
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || die "Missing command: $cmd"
}

package_version() {
  require_cmd cargo
  cargo pkgid | sed -E 's/^.*@([^ ]+)$/\1/'
}

build_zip() {
  require_cmd cargo
  require_cmd zip

  if ! cargo make --version >/dev/null 2>&1; then
    die "cargo-make is not installed. Run: cargo install cargo-make"
  fi

  log "Building $(basename "$OUT_ZIP")..."
  (cd "$ROOT_DIR" && cargo make pack)
  [[ -f "$OUT_ZIP" ]] || die "Not found: $OUT_ZIP"
  log "ZIP ready: $OUT_ZIP"
}

build_demo() {
  log "Building demo external processing..."
  [[ -f "$DEMO_EPF" ]] || die "Not found: $DEMO_EPF"
  [[ -f "$OUT_ZIP" ]] || die "Not found: $OUT_ZIP"
  cp "$OUT_ZIP" "$ROOT_DIR/demo/Demo/Templates/Компонента/Ext/Template.bin"
  log "Template updated: demo/Demo/Templates/Компонента/Ext/Template.bin"
  "$ROOT_DIR/scripts/build-epf.sh"
  log "Demo built: $DEMO_EPF"
}

main() {
  if [[ -z "$OUT_ZIP" ]]; then
    OUT_ZIP="$ROOT_DIR/out/WebTransportAddIn-$(package_version).zip"
  fi
  build_zip
  build_demo
}

main "$@"
