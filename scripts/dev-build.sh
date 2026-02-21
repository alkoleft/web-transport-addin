#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_XML="$ROOT_DIR/demo/Demo.xml"
DEMO_EPF="$ROOT_DIR/demo/Demo.epf"
OUT_DIR="$ROOT_DIR/out"
ENV_FILE="$ROOT_DIR/.env"
DEMO_TEMPLATE_BIN="$ROOT_DIR/demo/Demo/Templates/Компонента/Ext/Template.bin"
DEMO_COMPONENT_ZIP="$ROOT_DIR/out/WebTransportAddIn.zip"
BUILD_EPF_SCRIPT="$ROOT_DIR/scripts/build-epf.sh"

log() {
  printf '%s\n' "$*"
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

build_component() {
  log "Building component..."
  (cd "$ROOT_DIR" && cargo build --release)

  mkdir -p "$OUT_DIR"
  local uname_out
  uname_out="$(uname -s)"
  if [[ "$uname_out" == "Linux" ]]; then
    cp "$ROOT_DIR/target/release/libwebtransport.so" "$OUT_DIR/WebTransportAddIn_x64.so"
  elif [[ "$uname_out" == "Darwin" ]]; then
    cp "$ROOT_DIR/target/release/libwebtransport.dylib" "$OUT_DIR/WebTransportAddIn_x64.dylib"
  else
    cp "$ROOT_DIR/target/release/webtransport.dll" "$OUT_DIR/WebTransportAddIn_x64.dll"
  fi
  cp "$ROOT_DIR/Manifest.xml" "$OUT_DIR/"
  log "Component built (out/)."
}

update_demo_component_template() {
  log "Updating demo template with component archive..."
  [[ -f "$OUT_DIR/WebTransportAddIn_x64.so" ]] || die "Component not built: $OUT_DIR/WebTransportAddIn_x64.so"
  [[ -f "$ROOT_DIR/Manifest.xml" ]] || die "Missing Manifest.xml"

  mkdir -p "$(dirname "$DEMO_TEMPLATE_BIN")"
  zip -j -q "$DEMO_COMPONENT_ZIP" "$OUT_DIR/WebTransportAddIn_x64.so" "$ROOT_DIR/Manifest.xml"
  cp "$DEMO_COMPONENT_ZIP" "$DEMO_TEMPLATE_BIN"
  log "Template updated: $DEMO_TEMPLATE_BIN"
}

main() {
  if [[ -f "$ENV_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$ENV_FILE"
  fi
  build_component
  update_demo_component_template
  [[ -x "$BUILD_EPF_SCRIPT" ]] || die "Not executable: $BUILD_EPF_SCRIPT"
  "$BUILD_EPF_SCRIPT"
}

main "$@"
