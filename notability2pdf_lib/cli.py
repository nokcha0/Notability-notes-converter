from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .constants import DEFAULT_INPUT_DIR, DEFAULT_OUTPUT_DIR
from .converter import convert_note_folder_to_pdfs, convert_note_to_pdf


def default_output_path(input_path: Path) -> Path:
    return Path(DEFAULT_OUTPUT_DIR) / input_path.with_suffix(".pdf").name


def default_output_dir(input_path: Path) -> Path:
    return Path(DEFAULT_OUTPUT_DIR)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Convert Notability .note bundles into PDFs.")
    parser.add_argument(
        "input",
        type=Path,
        nargs="?",
        help=f"Path to the source .note file or folder (default: {DEFAULT_INPUT_DIR}/)",
    )
    parser.add_argument(
        "output",
        type=Path,
        nargs="?",
        help="Output PDF path for one file, or output folder for a source folder",
    )
    parser.add_argument(
        "--raster",
        action="store_true",
        help="Use the legacy full-page raster PDF renderer instead of the vector renderer",
    )
    parser.add_argument(
        "--keep-svg",
        action="store_true",
        help="Write per-page SVG overlays to output/svg/<input-name>/",
    )
    parser.add_argument(
        "--render-scale",
        type=float,
        default=2.0,
        help="Raster scale factor used by --raster before PDF export (default: 2.0)",
    )
    return parser.parse_args()


def run(args: argparse.Namespace) -> int:
    using_default_input = args.input is None
    input_path = args.input or Path(DEFAULT_INPUT_DIR)
    if not input_path.exists():
        if using_default_input:
            input_path.mkdir(parents=True, exist_ok=True)
            print(f"Created {input_path}; add .note files there, then run again.")
            return 0
        raise FileNotFoundError(f"Input path does not exist: {input_path}")

    if input_path.is_dir():
        output_dir = args.output or default_output_dir(input_path)
        written_paths = convert_note_folder_to_pdfs(
            input_path,
            output_dir,
            args.render_scale,
            raster=args.raster,
            keep_svg=args.keep_svg,
        )
        print(f"Mirrored {len(written_paths)} file(s) to {output_dir}")
        return 0

    output_path = args.output or default_output_path(input_path)
    svg_dir = None
    if args.keep_svg:
        svg_dir = Path(DEFAULT_OUTPUT_DIR) / "svg" / input_path.stem
    convert_note_to_pdf(input_path, output_path, args.render_scale, raster=args.raster, svg_dir=svg_dir)
    print(f"Wrote {output_path}")
    if svg_dir is not None:
        print(f"Wrote SVG overlays to {svg_dir}")
    return 0


def main() -> int:
    try:
        return run(parse_args())
    except (FileExistsError, FileNotFoundError, ValueError) as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
