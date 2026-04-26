# Notability `.note` Converter

Usage:

```bash
python notability2pdf.py "path/to/input.note" "path/to/output.pdf"
```

If you omit the output path, the script writes to `output/<input-name>.pdf`.

To convert a folder recursively while preserving the source tree:

```bash
python notability2pdf.py "path/to/notes-folder" "path/to/output-folder"
```

If you omit the output folder, the script writes to `output/<notes-folder-name>/`.
Every `.note` file is converted to a `.pdf` at the same relative path; non-note files are skipped.

The default renderer writes a vector PDF. Use `--keep-svg` to also write per-page SVG overlays to `output/svg/<input-name>/`. Use `--raster` if you need the previous full-page raster PDF path; `--render-scale` only applies with `--raster`.

What the converter reads from the bundle:

- `.note` is a ZIP bundle with a root folder.
- `Session.plist` is a keyed archive that contains document layout, typed text, media objects, and handwriting.
- `reflowState.pageWidthInDocumentCoordsKey` provides the document-space width.
- The note thumbnail, static paper model, or embedded PDF pages provide page geometry.
- `mediaObjects` provide placed images and their document-space bounds.
- `pdfFiles` and `pageLayoutArray` provide imported PDF pages.
- `Handwriting Overlay -> SpatialHash` provides stroke points, widths, RGBA colors, styles, and force data.

Current rendering path:

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
