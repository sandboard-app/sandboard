#!/usr/bin/env bash
# Thin face over Board /api/cockpit-session + OpenShell connect.
# Does not store lifecycle: environment and conversation live on the Board.
set -euo pipefail

USER="${SANDBOARD_USER:-}"
PASS="${SANDBOARD_PASSWORD:-}"

usage() {
  cat <<'EOF'
Usage: cockpit.sh <start|status|attach|park|resume|stop>

Board owns cockpit-session lifecycle. This script only calls REST (HTTP
Basic auth, no session state on disk) and `openshell sandbox connect`.

Env:
  SANDBOARD_URL          board origin (required) — same Host as the browser
  SANDBOARD_USER         admin username (prompted if unset)
  SANDBOARD_PASSWORD     admin password (prompted if unset)
EOF
}

: "${SANDBOARD_URL:?Set SANDBOARD_URL to your board origin (the URL in the browser)}"
SANDBOARD_URL="${SANDBOARD_URL%/}"

need_jq() {
  command -v jq >/dev/null || {
    echo "jq is required" >&2
    exit 1
  }
}

ensure_creds() {
  if [[ -z "$USER" ]]; then
    read -r -p "sandboard username: " USER
  fi
  if [[ -z "$PASS" ]]; then
    read -r -s -p "sandboard password: " PASS
    echo >&2
  fi
}

api() {
  local method=$1 path=$2
  shift 2
  ensure_creds
  curl -sS -u "${USER}:${PASS}" \
    -X "$method" \
    -H 'Content-Type: application/json' \
    "$@" \
    "${SANDBOARD_URL}${path}"
}

session_json() {
  api GET /api/cockpit-session
}

environment() {
  session_json | jq -r '.session.environment // empty'
}

cmd=${1:-}
case "$cmd" in
  start)
    need_jq
    api POST /api/cockpit-session -d '{}'
    echo >&2
    echo "Board session created; supervisor materializes the cockpit sandbox." >&2
    echo "Poll: $0 status" >&2
    ;;
  status)
    need_jq
    session_json | jq .
    ;;
  attach)
    need_jq
    env_name=$(environment)
    if [[ -z "$env_name" ]]; then
      echo "no session.environment yet — start the seat and wait for the supervisor" >&2
      session_json | jq . >&2
      exit 1
    fi
    echo "connecting to $env_name (Board still owns lifecycle)" >&2
    exec openshell sandbox connect "$env_name"
    ;;
  park)
    need_jq
    api POST /api/cockpit-session/park -d ''
    echo >&2
    ;;
  resume)
    need_jq
    api POST /api/cockpit-session/resume -d ''
    echo >&2
    ;;
  stop)
    need_jq
    api DELETE /api/cockpit-session
    echo "cockpit session cleared (supervisor stops cockpit agent + deletes sandbox)" >&2
    ;;
  ""|-h|--help|help)
    usage
    ;;
  *)
    echo "unknown command: $cmd" >&2
    usage >&2
    exit 1
    ;;
esac
