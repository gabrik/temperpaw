#!/usr/bin/env bash
# Dark Factory — one-shot local bootstrap (ADR-0066/0067/0069).
#
# Brings up everything needed to open the console and start a factory task:
#   1. prerequisite check (cargo, node >= 22, npm, python3, curl)
#   2. secret resolution (environment -> repo .env -> den demo .env fallback)
#   3. dark-factory WASM module build (wasm/build.sh)
#   4. temperpaw server on :3100 (reused if already healthy)
#   5. console user ensure (login, or first-boot register on a fresh deploy)
#   6. repository profiles seed (dark-factory-e2e + den, idempotent)
#   7. console build + vite preview on :8081 (reused if already serving)
#
# When it finishes: open http://localhost:8081, pick a repository, describe
# the task. The console auto-logs-in with the dev account below.
#
# Usage:
#   bash os-apps/dark-factory/scripts/bootstrap.sh [--skip-build] [--skip-den]
#
# Overrides (env):
#   TEMPER_URL / CONSOLE_URL        — default http://localhost:3100 / :8081
#   FACTORY_EMAIL / FACTORY_PASSWORD — console account (default: throwaway e2e account)
#   TENSORLAKE_API_KEY / ANTHROPIC_API_KEY / GITHUB_TOKEN (or GH_TOKEN)
#   DEMO_ENV                         — path to a fallback .env with those keys
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
APP_DIR="$REPO_ROOT/os-apps/dark-factory"
CONSOLE_DIR="$APP_DIR/console"

TEMPER_URL="${TEMPER_URL:-http://localhost:3100}"
CONSOLE_URL="${CONSOLE_URL:-http://localhost:8081}"
FACTORY_EMAIL="${FACTORY_EMAIL:-e2e@darkfactory.local}"
FACTORY_PASSWORD="${FACTORY_PASSWORD:-e2e-factory-pass}"
SERVER_LOG="${SERVER_LOG:-/tmp/temperpaw-3100.log}"
CONSOLE_LOG="${CONSOLE_LOG:-/tmp/vite-8081.log}"
COOKIES="/tmp/dark-factory-bootstrap-cookies.txt"
SKIP_BUILD=0
SEED_DEN=1
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    --skip-den) SEED_DEN=0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok() { printf '   ✓ %s\n' "$*"; }
die() { printf '\n\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# -- 1. prerequisites ----------------------------------------------------------
say "1/7 prerequisites"
for bin in cargo node npm python3 curl; do
  command -v "$bin" >/dev/null || die "$bin not found in PATH"
  ok "$bin"
done
node -e 'process.exit(parseInt(process.versions.node) >= 22 ? 0 : 1)' \
  || die "node >= 22 required (console engines), found $(node --version)"

# -- 2. secrets ----------------------------------------------------------------
say "2/7 secrets"
DEMO_ENV="${DEMO_ENV:-/Users/gabriele.baldoni/go/src/github.com/DataDog/experimental/users/shlomo.benyaminov/temper/den-software-factory/demo/.env}"

from_file() { # from_file KEY FILE -> value of first uncommented KEY= line
  local line
  line=$(grep -E "^$1=" "$2" 2>/dev/null | head -1 || true)
  local value="${line#*=}"
  value="${value%\"}"; value="${value#\"}"
  value="${value%\'}"; value="${value#\'}"
  printf '%s' "$value"
}

resolve_secret() { # resolve_secret OUTPUT_VAR FILE_KEY1 [FILE_KEY2 ...]
  local var="$1"; shift
  if [ -n "${!var:-}" ]; then ok "$var from environment"; return 0; fi
  local file key value
  for file in "$REPO_ROOT/.env" "$DEMO_ENV"; do
    [ -f "$file" ] || continue
    for key in "$@"; do
      value="$(from_file "$key" "$file")"
      if [ -n "$value" ]; then
        printf -v "$var" '%s' "$value"
        ok "$var from ${file#$HOME/} ($key)"
        return 0
      fi
    done
  done
  die "$var not found — export it, or add it to .env (keys: $*)"
}

resolve_secret TENSORLAKE_API_KEY TENSORLAKE_API_KEY TL_API_KEY
resolve_secret ANTHROPIC_API_KEY ANTHROPIC_API_KEY
resolve_secret GITHUB_TOKEN GITHUB_TOKEN GH_TOKEN
export TENSORLAKE_API_KEY TL_API_KEY="$TENSORLAKE_API_KEY" ANTHROPIC_API_KEY GITHUB_TOKEN
ok "TL_API_KEY / ANTHROPIC_API_KEY / GITHUB_TOKEN ready (values not echoed)"

# -- 3. WASM modules -----------------------------------------------------------
if [ "$SKIP_BUILD" -eq 0 ]; then
  say "3/7 dark-factory WASM modules"
  bash "$APP_DIR/wasm/build.sh" >/dev/null
  ok "6 modules built (factory_{janitor,validator,implementer,planner,deployer,publisher})"
else
  say "3/7 dark-factory WASM modules — skipped (--skip-build)"
fi

# -- 4. temperpaw server -------------------------------------------------------
say "4/7 temperpaw server ($TEMPER_URL)"
if curl -sf "$TEMPER_URL/healthz" >/dev/null 2>&1; then
  ok "already healthy — reusing (it keeps whatever secrets it booted with)"
else
  echo "   booting (first cargo build can take minutes; log: $SERVER_LOG)"
  (cd "$REPO_ROOT" && nohup env \
    PORT=3100 \
    TEMPER_EVENT_STORE=turso \
    TEMPER_PLATFORM_STORE=turso \
    TEMPER_QUERY_PROJECTION_STORE=turso \
    OTEL_ENABLED=false \
    TENSORLAKE_API_KEY="$TENSORLAKE_API_KEY" \
    TL_API_KEY="$TENSORLAKE_API_KEY" \
    ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
    GITHUB_TOKEN="$GITHUB_TOKEN" \
    cargo run -p temperpaw >"$SERVER_LOG" 2>&1 &)
  for _ in $(seq 1 120); do
    curl -sf "$TEMPER_URL/healthz" >/dev/null 2>&1 && break
    sleep 5
  done
  curl -sf "$TEMPER_URL/healthz" >/dev/null 2>&1 || die "server did not become healthy — see $SERVER_LOG"
  ok "server healthy"
fi

# -- 5. console user -----------------------------------------------------------
say "5/7 console user ($FACTORY_EMAIL)"
rm -f "$COOKIES"
if curl -sf -c "$COOKIES" -H 'Content-Type: application/json' \
    -d "{\"email\":\"$FACTORY_EMAIL\",\"password\":\"$FACTORY_PASSWORD\"}" \
    "$TEMPER_URL/auth/login" >/dev/null 2>&1; then
  ok "login works — account exists"
else
  echo "   login failed; trying first-boot register…"
  if curl -sf -c "$COOKIES" -H 'Content-Type: application/json' \
      -d "{\"email\":\"$FACTORY_EMAIL\",\"password\":\"$FACTORY_PASSWORD\"}" \
      "$TEMPER_URL/auth/register" >/dev/null 2>&1; then
    ok "registered as first account on this deployment"
  else
    die "login failed and register refused (an account already exists). Re-run with FACTORY_EMAIL/FACTORY_PASSWORD for the existing account."
  fi
fi

# -- 6. repository profiles ----------------------------------------------------
say "6/7 repository profiles (ADR-0069)"
export FACTORY_EMAIL FACTORY_PASSWORD
python3 "$SCRIPT_DIR/bootstrap_factory_repo.py" dark-factory-e2e --base-url "$TEMPER_URL"
if [ "$SEED_DEN" -eq 1 ]; then
  python3 "$SCRIPT_DIR/bootstrap_factory_repo.py" den --base-url "$TEMPER_URL"
else
  echo "   den profile skipped (--skip-den)"
fi

# -- 7. console ----------------------------------------------------------------
say "7/7 console ($CONSOLE_URL)"
if curl -sf "$CONSOLE_URL" >/dev/null 2>&1; then
  ok "already serving — reusing"
else
  cd "$CONSOLE_DIR"
  [ -d node_modules ] || { echo "   npm install…"; npm install --no-fund --no-audit >/dev/null; }
  echo "   building…"
  npm run build >/dev/null
  (nohup npm run preview >"$CONSOLE_LOG" 2>&1 &)
  for _ in $(seq 1 30); do
    curl -sf "$CONSOLE_URL" >/dev/null 2>&1 && break
    sleep 2
  done
  curl -sf "$CONSOLE_URL" >/dev/null 2>&1 || die "console did not start — see $CONSOLE_LOG"
  ok "console serving (log: $CONSOLE_LOG)"
fi

say "ready"
cat <<EOF
   Open  $CONSOLE_URL
   User  $FACTORY_EMAIL (the console auto-logs-in in dev)

   Then: New request → pick a repository (dark-factory-e2e$([ "$SEED_DEN" -eq 1 ] && echo " or den")) →
   describe the task. The factory plans, implements, validates with the
   profile's real gates, and waits at the human approval gates.
EOF
