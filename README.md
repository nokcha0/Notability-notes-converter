# Notability `.note` Converter

Converts `.note` to `.pdf` by default, or optionally to `.svg`

Initially made for personal use as I couldn't find a reliable converter online. I hope this is useful for someone.

## Where is this useful?

- Convert `.note` to other file formats in bulk (with high speed)
- View `.note` files on a machine without Notability
- Switch platforms `.note` -> `.svg` (SVGs can be imported in other note taking apps as editable files)

## How to use

Requirements: Rust

Put your exported `.note` files under `input/`, then run this from the repo root:

```bash
cargo run
```

That converts everything under `input/` into `output/`.
Nested folder structures are respected.
Example:

```text
.
├── input/
│   ├── example1.note
│   └── example_folder/
│       └── example2.note
└── output/
    ├── example1.pdf
    └── example_folder/
        └── example2.pdf
```

## Optional Flags: Format, Input / Output Folder

```bash
cargo run -- --format svg --input path/to/input --output path/to/output
```

## Analysis of .note file format

Thanks to [Julia Evans' blog](https://jvns.ca/blog/2018/03/31/reverse-engineering-notability-format/), I was able to quickly grasp how the file format works.

A `.note` file is a ZIP archive.

- `Session.plist`: the main document object graph
- `PDFs/`: imported PDF source files
- image assets referenced by embedded figures
- thumbnail PNGs for page aspect ratio fallback

`Session.plist` is not a flat plist. It is an Apple keyed archive with `$objects` and `$top`, so decoding it requires following UID references through the object table.

The main content is stored under:

- `richText`: the main content container
- `richText -> reflowState`: carries layout information such as `pageWidthInDocumentCoordsKey`
- `richText -> attributedString`: stores the plain text and style subranges
- `richText -> mediaObjects`: stores placed images with document coordinates, size, and z-order
- `richText -> pageLayoutArray`: maps note pages to imported PDF files and source page indices
- `richText -> Handwriting Overlay -> SpatialHash`: stores the pen stroke payloads

The handwriting payload in `SpatialHash` is mostly little-endian binary arrays:

- `curvespoints`: `f32` pairs representing stroke points
- `curvesnumpoints`: `i32` point counts for each stroke
- `curveswidth`: base stroke widths as `f32`
- `curvesforces`: pressure samples as `f32`
- `curvesfractionalwidths`: width multipliers as `f32`
- `curvescolors`: RGBA bytes
- `curvesstyles`: per-stroke tool/style identifiers

Many strokes follow a cubic Bezier layout where the point count matches `1 + 3n`, so the data is not just a plain polyline list.

Page geometry is reconstructed from:

- explicit document width from `reflowState`
- imported PDF page boxes when the note is PDF-backed
- thumbnail aspect ratio as a fallback when no embedded PDF page defines the height
- paper attributes such as line style, paper size, and sizing behavior

That is enough to reconstruct the sample notes.
