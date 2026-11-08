#!/usr/bin/env bash
# Builds both unmodified upstream renderers (Vue UIs): poe1 from the
# awakened-poe-trade submodule, poe2 from exiled-exchange-2. Run this
# whenever either submodule is updated.
set -euo pipefail

root="$(dirname "${BASH_SOURCE[0]}")/.."

for vendor in awakened-poe-trade exiled-exchange-2; do
  (
    cd "$root/vendor/$vendor/renderer"
    npm ci
    # poe2's renderer generates its data/*/…index.bin lookups (gitignored
    # there, committed in poe1's); without them it crashes on startup.
    npm run --if-present make-index-files
    npm run build
  )
  echo "renderer built at vendor/$vendor/renderer/dist/"
done
