#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
node "$ROOT_DIR/scripts/test-renovate-matchers.mjs" \
  "$ROOT_DIR/.github/renovate/nyl-helm-components.json5" \
  "$ROOT_DIR/integrationtests/fixtures/renovate-matchers"
