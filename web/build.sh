#!/bin/sh
# Requires wasm32-unknown-unknown, wasm-bindgen-cli 0.2.118, and Node.js 22.13+.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out=${1:-"$root/web/dist"}
mkdir -p "$out"
out=$(CDPATH= cd -- "$out" && pwd)

cd "$root"
if [ "$(wasm-bindgen --version)" != "wasm-bindgen 0.2.118" ]; then
  echo 'Install wasm-bindgen-cli 0.2.118.' >&2
  exit 1
fi
cargo build --locked --profile wasm --target wasm32-unknown-unknown -p notability-web
target=${CARGO_TARGET_DIR:-"$root/target"}
case "$target" in /*) ;; *) target="$root/$target" ;; esac
wasm-bindgen --target web --no-typescript --out-dir "$out/pkg" \
  "$target/wasm32-unknown-unknown/wasm/notability_web.wasm"

cd "$root/web"
npm ci --no-audit --no-fund --ignore-scripts --omit=optional
cp site/* "$out/"
mkdir -p "$out/vendor/pdfjs"
cp node_modules/pdfjs-dist/build/pdf.mjs node_modules/pdfjs-dist/build/pdf.worker.mjs \
  node_modules/pdfjs-dist/LICENSE "$out/vendor/pdfjs/"
cp -R node_modules/pdfjs-dist/standard_fonts node_modules/pdfjs-dist/cmaps \
  node_modules/pdfjs-dist/wasm node_modules/pdfjs-dist/iccs "$out/vendor/pdfjs/"
touch "$out/.nojekyll"
node publish.mjs "$out"
