#!/usr/bin/env bash
# Build all dark-factory WASM modules (wasm32-unknown-unknown, release).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/../../wasm-build-env.sh"

for module in factory_janitor factory_validator factory_implementer factory_planner factory_publisher; do
  echo "==> $module"
  (cd "$SCRIPT_DIR/$module" && cargo build --target wasm32-unknown-unknown --release)
  cp "$SCRIPT_DIR/$module/target/wasm32-unknown-unknown/release/$module.wasm" "$SCRIPT_DIR/$module/$module.wasm"
  echo "  -> $module built successfully"
done
