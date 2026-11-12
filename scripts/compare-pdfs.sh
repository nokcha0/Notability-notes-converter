#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 <generated.pdf> <target.pdf> [report-directory]" >&2
  exit 2
fi

generated_pdf=$(realpath "$1")
target_pdf=$(realpath "$2")
report_dir=${3:-pdf-comparison}
dpi=${PDF_COMPARE_DPI:-144}

for command in pdfinfo pdftoppm magick; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "missing required command: $command (enter nix-shell first)" >&2
    exit 2
  fi
done

mkdir -p "$report_dir"
report_dir=$(realpath "$report_dir")
work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT

generated_pages=$(pdfinfo "$generated_pdf" | awk '/^Pages:/ { print $2 }')
target_pages=$(pdfinfo "$target_pdf" | awk '/^Pages:/ { print $2 }')
if [[ -z "$generated_pages" || -z "$target_pages" ]]; then
  echo "could not determine PDF page counts" >&2
  exit 1
fi

page_count=$generated_pages
if (( target_pages < page_count )); then
  page_count=$target_pages
fi

summary="$report_dir/summary.tsv"
printf 'page\trmse\tpixel_similarity\tssim_error\tstructural_similarity\n' >"$summary"

for ((page = 1; page <= page_count; page++)); do
  generated_png="$work_dir/generated-$page.png"
  target_png="$work_dir/target-$page.png"
  pdftoppm -f "$page" -l "$page" -r "$dpi" -png -singlefile \
    "$generated_pdf" "$work_dir/generated-$page" >/dev/null 2>&1
  pdftoppm -f "$page" -l "$page" -r "$dpi" -png -singlefile \
    "$target_pdf" "$work_dir/target-$page" >/dev/null 2>&1

  generated_size=$(magick identify -format '%wx%h' "$generated_png")
  target_size=$(magick identify -format '%wx%h' "$target_png")
  if [[ "$generated_size" != "$target_size" ]]; then
    echo "page $page dimensions differ: generated=$generated_size target=$target_size" >&2
    exit 1
  fi

  cp "$generated_png" "$report_dir/page-$page-generated.png"
  cp "$target_png" "$report_dir/page-$page-target.png"

  rmse_output=$(magick compare -metric RMSE "$target_png" "$generated_png" null: 2>&1 || true)
  rmse=$(awk -F'[()]' '{ print $2 }' <<<"$rmse_output")
  pixel_similarity=$(awk -v rmse="$rmse" 'BEGIN { printf "%.8f", 1.0 - rmse }')
  ssim_output=$(magick compare -metric SSIM "$target_png" "$generated_png" null: 2>&1 || true)
  ssim_error=$(awk -F'[()]' '{ print $2 }' <<<"$ssim_output")
  structural_similarity=$(awk -v error="$ssim_error" 'BEGIN { printf "%.8f", 1.0 - error }')

  printf '%d\t%s\t%s\t%s\t%s\n' \
    "$page" "$rmse" "$pixel_similarity" "$ssim_error" "$structural_similarity" >>"$summary"

  magick "$target_png" "$generated_png" +append \
    "$report_dir/page-$page-side-by-side.png"
  magick "$target_png" "$generated_png" -compose difference -composite \
    -colorspace gray -auto-level "$report_dir/page-$page-difference.png"
done

average_pixel_similarity=$(awk 'NR > 1 { total += $3; count++ } END { if (count) printf "%.8f", total / count; else print "0" }' "$summary")
average_structural_similarity=$(awk 'NR > 1 { total += $5; count++ } END { if (count) printf "%.8f", total / count; else print "0" }' "$summary")
{
  printf 'generated\t%s\n' "$generated_pdf"
  printf 'target\t%s\n' "$target_pdf"
  printf 'dpi\t%s\n' "$dpi"
  printf 'generated_pages\t%s\n' "$generated_pages"
  printf 'target_pages\t%s\n' "$target_pages"
  printf 'average_pixel_similarity\t%s\n' "$average_pixel_similarity"
  printf 'average_structural_similarity\t%s\n' "$average_structural_similarity"
} >"$report_dir/metadata.tsv"

cat "$summary"
printf 'average pixel similarity: %s\n' "$average_pixel_similarity"
printf 'average structural similarity: %s\n' "$average_structural_similarity"
printf 'visual report: %s\n' "$report_dir"

if [[ "$generated_pages" != "$target_pages" ]]; then
  echo "page-count mismatch: generated=$generated_pages target=$target_pages" >&2
  exit 1
fi
