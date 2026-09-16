#!/usr/bin/env bash
# Boot temperpaw on :3100 with dark-factory e2e secrets from the den demo .env.
# Usage: bash scripts/boot-e2e-3100.sh
set -euo pipefail

DEMO_ENV="/Users/gabriele.baldoni/go/src/github.com/DataDog/experimental/users/shlomo.benyaminov/temper/den-software-factory/demo/.env"

getvar() {
  local key="$1"
  local line
  line=$(rg "^${key}=" "$DEMO_ENV" | head -1)
  local value="${line#*=}"
  value="${value%\"}"; value="${value#\"}"
  value="${value%\'}"; value="${value#\'}"
  printf '%s' "$value"
}

cd "$(dirname "$0")/.."

exec env -i HOME="$HOME" PATH="$PATH" \
  PORT=3100 \
  TEMPER_EVENT_STORE=turso \
  TEMPER_PLATFORM_STORE=turso \
  TEMPER_QUERY_PROJECTION_STORE=turso \
  OTEL_ENABLED=false \
  TENSORLAKE_API_KEY="$(getvar TENSORLAKE_API_KEY)" \
  TL_API_KEY="$(getvar TENSORLAKE_API_KEY)" \
  ANTHROPIC_API_KEY="$(getvar ANTHROPIC_API_KEY)" \
  GITHUB_TOKEN="$(getvar GH_TOKEN)" \
  cargo run -p temperpaw
