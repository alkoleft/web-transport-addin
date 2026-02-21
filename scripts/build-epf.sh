#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$ROOT_DIR/.env"
DEMO_XML="$ROOT_DIR/demo/Demo.xml"
DEMO_EPF="$ROOT_DIR/demo/Demo.epf"

log() {
  printf '%s\n' "$*"
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

build_demo() {
  log "Building demo external processing from Demo.xml..."
  [[ -f "$DEMO_XML" ]] || die "Not found: $DEMO_XML"

  local onec_designer onec_ib
  onec_designer="${ONEC_DESIGNER:-/opt/1cv8/x86_64/8.5.1.1150/1cv8}"
  onec_ib="${ONEC_IB_PATH:-/home/alko/develop/onec_file_db/EMPTY}"

  [[ -n "$onec_designer" ]] || die "Set ONEC_DESIGNER to 1C executable (e.g. /opt/1C/v8.3/x86_64/1cv8)"
  [[ -n "$onec_ib" ]] || die "Set ONEC_IB_PATH to an existing infobase directory"

  local log_file
  log_file="${ONEC_LOG_PATH:-demo/build_demo.log}"

  local auth=()
  if [[ -n "${ONEC_USER:-}" ]]; then
    auth+=(/N"${ONEC_USER}")
  fi
  if [[ -n "${ONEC_PASS:-}" ]]; then
    auth+=(/P"${ONEC_PASS}")
  fi

  (
    cd "$ROOT_DIR"
    "$onec_designer" DESIGNER /F"$onec_ib" /DisableStartupDialogs \
      "${auth[@]}" \
      /LoadExternalDataProcessorOrReportFromFiles "demo/Demo.xml" "demo/Demo.epf" \
      /Out "$log_file"
  )

  log "Demo built: $DEMO_EPF"
}

main() {
  if [[ -f "$ENV_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$ENV_FILE"
  fi
  build_demo
}

main "$@"
