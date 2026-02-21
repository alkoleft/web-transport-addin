#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$ROOT_DIR/.env"
DEMO_EPF="$ROOT_DIR/demo/Demo.epf"

log() {
  printf '%s\n' "$*"
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

main() {
  if [[ -f "$ENV_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$ENV_FILE"
  fi

  [[ -f "$DEMO_EPF" ]] || die "Not found: $DEMO_EPF (run scripts/build.sh)"

  local onec_enterprise onec_ib log_file
  onec_enterprise="${ONEC_ENTERPRISE:-${ONEC_DESIGNER:-/opt/1cv8/x86_64/8.5.1.1150/1cv8}}"
  onec_ib="${ONEC_IB_PATH:-/home/alko/develop/onec_file_db/EMPTY}"
  log_file="${ONEC_LOG_PATH:-demo/run_demo.log}"

  [[ -n "$onec_enterprise" ]] || die "Set ONEC_ENTERPRISE to 1C executable (e.g. /opt/1C/v8.3/x86_64/1cv8)"
  [[ -n "$onec_ib" ]] || die "Set ONEC_IB_PATH to an existing infobase directory"

  local auth=()
  if [[ -n "${ONEC_USER:-}" ]]; then
    auth+=(/N"${ONEC_USER}")
  fi
  if [[ -n "${ONEC_PASS:-}" ]]; then
    auth+=(/P"${ONEC_PASS}")
  fi

  log "Running demo external processing..."
  (
    cd "$ROOT_DIR"
    "$onec_enterprise" ENTERPRISE /F"$onec_ib" /DisableStartupDialogs \
      "${auth[@]}" \
      /Execute "$DEMO_EPF" \
      /Out "$log_file" \
      /RunModeManagedApplication
  )

  log "Done. Log: $log_file"
}

main "$@"
