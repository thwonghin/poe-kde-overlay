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

# poe1: base types upstream's data doesn't have yet (tools/poe1-data/extra,
# `poe1-data extra-items`), appended to the built data. The lookups over it
# (*.index.bin, string offsets) are rebuilt by upstream's own, unmodified
# make-index-files.mjs, run from a scratch tree where its ../../public/data
# is the built data.
renderer="$(realpath "$root/vendor/awakened-poe-trade/renderer")"
for extra in "$root"/tools/poe1-data/extra/*/items.ndjson; do
  cat "$extra" >> "$renderer/dist/data/$(basename "$(dirname "$extra")")/items.ndjson"
done
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/src/assets" "$scratch/public"
cp "$renderer/src/assets/make-index-files.mjs" "$scratch/src/assets/"
ln -s "$renderer/dist/data" "$scratch/public/data"
ln -s "$renderer/node_modules" "$scratch/node_modules"
node "$scratch/src/assets/make-index-files.mjs"
echo "poe1 extra items added"
