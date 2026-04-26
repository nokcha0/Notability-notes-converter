# Notability `.note` Converter

Convert Notability `.note` files to PDF while preserving a folder tree.

## Usage

Put your exported `.note` files and any other files under `input/`, then run:

```bash
python notability2pdf.py
```

The converter writes to `output/`. The output tree mirrors `input/`: every non-note file is copied as-is, and every `.note` file is replaced by a `.pdf` at the same relative path.
For folder conversion, the output folder is regenerated each run so stale files from previous runs do not remain. Do not store unrelated files in the output folder.

Example:

```text
input/Semester 1/Week 1/Lecture.note
input/Semester 1/Week 1/handout.txt
input/Semester 1/Week 2/Review.note
```

becomes:

```text
output/Semester 1/Week 1/Lecture.pdf
output/Semester 1/Week 1/handout.txt
output/Semester 1/Week 2/Review.pdf
```

You can still pass explicit paths:

```bash
python notability2pdf.py "path/to/notes-folder" "path/to/output-folder"
python notability2pdf.py "path/to/input.note" "path/to/output.pdf"
```

If you pass a folder and omit the output folder, the script writes to `output/`.
If you pass one `.note` file and omit the output path, the script writes to `output/<input-name>.pdf`.

The default renderer writes a vector PDF. Use `--raster` for the older full-page raster path; `--render-scale` only applies with `--raster`. Use `--keep-svg` only for debugging, because it writes extra SVG overlay files and makes the output folder no longer a pure mirror.

If two input files would map to the same output path, for example `foo.note` and `foo.pdf`, the converter stops with an output collision error instead of overwriting one.
The output folder must be separate from the input folder and must not contain the input folder.

## `.note` Format Notes

What reverse engineering found:

- `.note` is a ZIP bundle with a root folder.
- `Session.plist` is an Apple binary plist keyed archive. It contains document layout, typed text, media objects, imported PDFs, and handwriting.
- `reflowState.pageWidthInDocumentCoordsKey` provides the document-space width.
- The note thumbnail, static paper model, or embedded PDF pages provide page geometry.
- `mediaObjects` provide placed images and their document-space bounds.
- `pdfFiles` and `pageLayoutArray` provide imported PDF pages.
- `Handwriting Overlay -> SpatialHash` provides stroke point blobs, point counts, widths, colors, styles, pressure/force data, and fractional widths.
- Numeric blobs are little-endian arrays: 32-bit floats for points, widths, forces, and fractional widths; 32-bit integers for point counts; bytes for styles; 32-bit RGBA values for colors.
- Handwriting point counts usually follow `1 + 3n`. That means points are cubic Bezier data: start point, then repeated control1/control2/end triples. Treating them as a polyline makes handwriting jagged and less readable.
- Force and fractional-width blobs have one sample per Bezier anchor, not one sample per raw control point. Pencil strokes use those samples to vary alpha and width.
- Stroke styles currently observed: pen, highlighter, pencil, and not-exported.

## Reverse-Engineering Steps

Useful process for inspecting new `.note` files:

1. Run `file sample.note` and unzip it. It should be a ZIP archive.
2. Locate `Session.plist` inside the bundle root.
3. Load the binary plist with Python `plistlib` and resolve Apple keyed-archive `UID` references.
4. Find the root `richText` object, then `Handwriting Overlay -> SpatialHash`.
5. Inspect blob lengths. `curvesnumpoints` count should match `curveswidth`, `curvescolors`, and `curvesstyles`. `curvespoints` should contain twice the total point count because points are `(x, y)` float pairs.
6. Check whether each curve point count is `1 + 3n`. If yes, render as cubic Bezier segments instead of connecting every point.
7. Match `curvesforces` and `curvesfractionalwidths` to Bezier anchor sample counts. If that length does not match, fall back to resampling.
8. Compare rendered output against Notability-exported PDFs by rasterizing both at the same DPI and measuring pixel difference.

## Rendering Path

- vector PDF output via Cairo and pypdf
- optional SVG page overlays
- imported PDF pages merged as vector PDF backgrounds
- ruled paper backgrounds for `Lines:*:*:*` styles
- flowed typed text
- embedded image media objects
- transparent highlighters with Notability-like darken blending
- pressure-sensitive pencil strokes
- handwriting curves with Notability's page content inset

The implementation is tuned against the sample note in `examples/` and reproduces the same page geometry as the exported sample PDF.
